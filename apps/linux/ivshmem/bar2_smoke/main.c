/*
 * ivshmem shared-memory smoke scenario.
 *
 * This program owns only payload, checksum, marker, and timeout diagnostics;
 * every device interaction goes through the libivshmem adapter so the smoke
 * test never hard-codes a BDF, a BAR address, or a sysfs path.
 *
 * Output contract (frozen with the shared QEMU case):
 *   success: "ivshmem <backend> pass"
 *   failure: "ivshmem <backend> failed <step>: <detail>"
 *   progress: "ivshmem checkpoint <name>"
 *
 * The polling handshake exercises discovery without a fixed BDF, mapping
 * without absolute addresses, profile registers, a BAR2 payload round-trip,
 * and the doorbell/Event Status path: the peer rings its own doorbell and
 * waits through the polling backend twice (post-clear re-pend), then
 * verifies that an unsupported vector produces no event.
 *
 * With --cross-peer the smoke additionally runs the dual-peer handshake:
 * the initiator (peer 0) publishes a request mailbox in its own output
 * section (readable by the responder through the per-peer output
 * permissions), publishes its handshake state and rings the responder's
 * doorbell; the responder validates the request, publishes its reply and
 * rings back. The initiator's final "ivshmem polling pass" therefore
 * implies a completed cross-peer round trip; the responder prints the
 * distinct "ivshmem polling relay pass" instead.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ivshmem.h"

/* Payload lives inside this peer's output section. The frozen F5 BAR2
 * layout gives every peer one 28 KiB output page-range starting after the
 * state-table page (peer N: 0x1000 + N * 0x7000), so the smoke writes only
 * bytes it owns. */
#define SMOKE_OUTPUT_SECTION_BASE 0x1000
#define SMOKE_OUTPUT_SECTION_STRIDE 0x7000
#define SMOKE_PAYLOAD_SIZE 0x100

/* Cross-peer mailbox magic ("IVCP") and the BAR0 state values each side
 * publishes for the handshake; the remote peer reads them from the state
 * table (F4 remote-observation evidence). */
#define SMOKE_MAILBOX_MAGIC 0x49564350u
#define SMOKE_HANDSHAKE_STATE_SELF 0x00010002u
#define SMOKE_HANDSHAKE_STATE_INITIATOR 0x00010003u
#define SMOKE_HANDSHAKE_STATE_RESPONDER 0x00010004u

/* Guest scheduling is not part of the device contract: the handshake waits
 * long enough for the other guest to reach the smoke, and every wait
 * failure carries a distinct step name. */
#define SMOKE_HANDSHAKE_TIMEOUT_MS 30000

static const uint32_t EXPECTED_MAX_PEERS = 2;
static const char *selected_backend = "unknown";

static void checkpoint(const char *name)
{
    printf("ivshmem checkpoint %s\n", name);
    fflush(stdout);
}

static void fail(const char *step, const char *detail)
{
    printf("ivshmem %s failed %s: %s\n", selected_backend, step, detail);
    fflush(stdout);
    exit(1);
}

static void fail_err(const char *step, int err)
{
    fail(step, ivshmem_strerror(err));
}

static void usage(const char *program)
{
    fprintf(stderr,
            "usage: %s --backend polling|interrupt [--cross-peer] [--bdf <BDF>]\n",
            program);
}

struct options {
    const char *bdf;
    enum ivshmem_backend_kind backend;
    int cross_peer;
};

static void parse_options(int argc, char **argv, struct options *options)
{
    int index;

    memset(options, 0, sizeof(*options));
    options->backend = IVSHMEM_BACKEND_POLLING;
    selected_backend = "polling";
    for (index = 1; index < argc; index++) {
        if (strcmp(argv[index], "--backend") == 0 && index + 1 < argc) {
            index++;
            if (strcmp(argv[index], "polling") == 0) {
                options->backend = IVSHMEM_BACKEND_POLLING;
                selected_backend = "polling";
            } else if (strcmp(argv[index], "interrupt") == 0) {
                options->backend = IVSHMEM_BACKEND_INTERRUPT;
                selected_backend = "interrupt";
            } else {
                fail("backend", "backend must be polling or interrupt");
            }
        } else if (strcmp(argv[index], "--bdf") == 0 && index + 1 < argc) {
            index++;
            options->bdf = argv[index];
        } else if (strcmp(argv[index], "--cross-peer") == 0) {
            options->cross_peer = 1;
        } else {
            usage(argv[0]);
            fail("args", "unrecognized command line");
        }
    }
}

static uint32_t payload_checksum(const uint8_t *payload, size_t size)
{
    uint32_t checksum = 0x49565348u;
    size_t index;

    for (index = 0; index < size; index++) {
        checksum = checksum * 31u + payload[index];
    }
    return checksum;
}

static void exchange_payload(void *shared, uint32_t peer_id)
{
    size_t payload_offset =
        SMOKE_OUTPUT_SECTION_BASE + (size_t)peer_id * SMOKE_OUTPUT_SECTION_STRIDE;
    uint8_t payload[SMOKE_PAYLOAD_SIZE];
    volatile uint8_t *remote = (volatile uint8_t *)shared + payload_offset;
    uint32_t written_checksum;
    uint32_t readback_checksum;
    size_t index;

    for (index = 0; index < sizeof(payload); index++) {
        payload[index] = (uint8_t)(index * 7 + 0x5a);
    }
    written_checksum = payload_checksum(payload, sizeof(payload));

    for (index = 0; index < sizeof(payload); index++) {
        remote[index] = payload[index];
    }
    for (index = 0; index < sizeof(payload); index++) {
        payload[index] = remote[index];
    }
    readback_checksum = payload_checksum(payload, sizeof(payload));
    if (written_checksum != readback_checksum) {
        fail("shared-memory", "BAR2 payload checksum mismatch");
    }
    printf("ivshmem checkpoint payload offset=%zx\n", payload_offset);
    fflush(stdout);
}

/* Cross-peer mailbox exchanged through the two output sections: each peer
 * writes only its own section and reads the other's, matching the F5
 * ownership rules. */
struct smoke_mailbox {
    uint32_t magic;
    uint32_t checksum;
    uint32_t reserved[2];
    uint8_t payload[SMOKE_PAYLOAD_SIZE];
};

static volatile struct smoke_mailbox *section_mailbox(void *shared,
                                                      uint32_t peer_id)
{
    size_t offset =
        SMOKE_OUTPUT_SECTION_BASE + (size_t)peer_id * SMOKE_OUTPUT_SECTION_STRIDE;

    return (volatile struct smoke_mailbox *)((volatile uint8_t *)shared + offset);
}

static uint32_t mailbox_payload_checksum(
    const volatile struct smoke_mailbox *mailbox)
{
    uint8_t payload[SMOKE_PAYLOAD_SIZE];
    size_t index;

    for (index = 0; index < sizeof(payload); index++) {
        payload[index] = mailbox->payload[index];
    }
    return payload_checksum(payload, sizeof(payload));
}

static void fill_mailbox(volatile struct smoke_mailbox *mailbox, uint32_t seed)
{
    size_t index;

    for (index = 0; index < sizeof(mailbox->payload); index++) {
        mailbox->payload[index] = (uint8_t)(index * 7 + seed);
    }
    mailbox->magic = SMOKE_MAILBOX_MAGIC;
    mailbox->checksum = mailbox_payload_checksum(mailbox);
}

static void validate_mailbox(const volatile struct smoke_mailbox *mailbox,
                             const char *step)
{
    if (mailbox->magic != SMOKE_MAILBOX_MAGIC) {
        fail(step, "cross-peer mailbox magic mismatch");
    }
    if (mailbox->checksum != mailbox_payload_checksum(mailbox)) {
        fail(step, "cross-peer payload checksum mismatch");
    }
}

static void wait_remote_state(const volatile uint32_t *state_table,
                              uint32_t peer_id, uint32_t expected,
                              const char *step)
{
    uint32_t spins;
    int attempt;

    /* The remote peer publishes its handshake state through BAR0 before it
     * rings the doorbell, but Normal-memory stores do not order against the
     * subsequent Device write, so poll briefly instead of assuming the
     * value is visible once the event arrives. The state write only trails
     * the event by one coherency transaction, so the loop normally exits on
     * the first observation. */
    for (attempt = 0; attempt < 100; attempt++) {
        if (state_table[peer_id] == expected) {
            return;
        }
        for (spins = 0; spins < 100000u; spins++) {
            /* Compiler barrier: keep the observation loop real. */
            __asm__ volatile ("" ::: "memory");
        }
    }
    fail(step, "remote peer state did not reach the handshake value");
}

static void cross_peer_exchange(void *shared, uint32_t peer_id,
                                struct ivshmem_device *dev,
                                struct ivshmem_backend *backend)
{
    const volatile uint32_t *state_table = (const volatile uint32_t *)shared;
    volatile struct smoke_mailbox *own = section_mailbox(shared, peer_id);
    uint32_t target = peer_id ^ 1u;
    volatile struct smoke_mailbox *remote = section_mailbox(shared, target);
    int wait_result;

    if (peer_id == 0) {
        /* Initiator: publish the request, ring the responder, then validate
         * the responder's published state and reply. */
        fill_mailbox(own, 0x5a);
        ivshmem_write_reg32(dev, IVSHMEM_REG_STATE,
                            SMOKE_HANDSHAKE_STATE_INITIATOR);
        /* The doorbell is a Device write and does not order the prior
         * Normal-memory mailbox and state stores. */
        __sync_synchronize();
        ivshmem_write_reg32(dev, IVSHMEM_REG_DOORBELL, (target << 16) | 0u);
        checkpoint("cross-peer-request");

        wait_result =
            ivshmem_backend_wait_event(backend, SMOKE_HANDSHAKE_TIMEOUT_MS);
        if (wait_result != 1) {
            fail("cross-peer", wait_result == 0 ? "reply event timed out"
                                                : "event wait failed");
        }
        wait_remote_state(state_table, target,
                          SMOKE_HANDSHAKE_STATE_RESPONDER, "cross-peer");
        validate_mailbox(remote, "cross-peer");
        checkpoint("cross-peer-reply");
    } else {
        /* Responder: the first event must be the initiator's request. The
         * self-doorbell tests run only after the exchange, because a
         * pending self event would merge with the request event (Event
         * Status is one merged bit). */
        wait_result =
            ivshmem_backend_wait_event(backend, SMOKE_HANDSHAKE_TIMEOUT_MS);
        if (wait_result != 1) {
            fail("cross-peer", wait_result == 0 ? "request event timed out"
                                                : "event wait failed");
        }
        wait_remote_state(state_table, target,
                          SMOKE_HANDSHAKE_STATE_INITIATOR, "cross-peer");
        validate_mailbox(remote, "cross-peer");

        fill_mailbox(own, 0xa5);
        ivshmem_write_reg32(dev, IVSHMEM_REG_STATE,
                            SMOKE_HANDSHAKE_STATE_RESPONDER);
        __sync_synchronize();
        ivshmem_write_reg32(dev, IVSHMEM_REG_DOORBELL, (target << 16) | 0u);
        checkpoint("cross-peer-relay");
    }
}

static void self_doorbell_tests(struct ivshmem_device *dev,
                                struct ivshmem_backend *backend,
                                uint32_t peer_id)
{
    int wait_result;

    /* Ring the doorbell for this endpoint itself: the target Event Status
     * must pend, the polling backend must clear it via W1C, and a second
     * doorbell must pend again. */
    ivshmem_write_reg32(dev, IVSHMEM_REG_DOORBELL,
                        (peer_id << 16) | 0u);
    wait_result = ivshmem_backend_wait_event(backend, 5000);
    if (wait_result != 1) {
        fail("doorbell", wait_result == 0 ? "first event timed out"
                                          : "event wait failed");
    }
    if ((ivshmem_read_reg32(dev, IVSHMEM_REG_EVENT_STATUS) & 1) != 0) {
        fail("doorbell", "event status was not cleared by the wait");
    }
    checkpoint("doorbell-first");

    ivshmem_write_reg32(dev, IVSHMEM_REG_DOORBELL, (peer_id << 16) | 0u);
    wait_result = ivshmem_backend_wait_event(backend, 5000);
    if (wait_result != 1) {
        fail("doorbell", wait_result == 0 ? "second event timed out"
                                          : "event wait failed");
    }
    checkpoint("doorbell-second");
}

int main(int argc, char **argv)
{
    struct options options;
    struct ivshmem_device *dev = NULL;
    struct ivshmem_backend *backend = NULL;
    void *registers = NULL;
    void *shared = NULL;
    size_t register_size = 0;
    size_t shared_size = 0;
    uint32_t peer_id;
    uint32_t max_peers;

    parse_options(argc, argv, &options);

    int result = ivshmem_find_device(options.bdf, &dev);
    if (result != IVSHMEM_OK) {
        fail_err("discover", result);
    }
    checkpoint("discover");

    result = ivshmem_enable_device(dev);
    if (result != IVSHMEM_OK) {
        fail_err("enable", result);
    }
    checkpoint("enable");

    result = ivshmem_map_bar(dev, IVSHMEM_BAR_REGISTERS, &registers,
                             &register_size);
    if (result != IVSHMEM_OK) {
        fail_err("map-registers", result);
    }
    if (register_size < IVSHMEM_REG_PAGE_SIZE) {
        fail("map-registers", "register BAR is smaller than one page");
    }
    result = ivshmem_map_bar(dev, IVSHMEM_BAR_SHARED, &shared, &shared_size);
    if (result != IVSHMEM_OK) {
        fail_err("map-shared", result);
    }
    checkpoint("map");

    peer_id = ivshmem_read_reg32(dev, IVSHMEM_REG_ID);
    max_peers = ivshmem_read_reg32(dev, IVSHMEM_REG_MAX_PEERS);
    if (max_peers != EXPECTED_MAX_PEERS) {
        char detail[128];

        snprintf(detail, sizeof(detail),
                 "max_peers reads %u (peer_id reads %u), expected %u",
                 (unsigned)max_peers, (unsigned)peer_id,
                 (unsigned)EXPECTED_MAX_PEERS);
        fail("profile", detail);
    }
    printf("ivshmem checkpoint profile peer_id=%u max_peers=%u "
           "shared_bytes=%zu\n",
           (unsigned)peer_id, (unsigned)max_peers, shared_size);

    if (ivshmem_shared_memory(dev, &shared_size) != shared) {
        fail("map-shared", "shared-memory mapping is not cached");
    }
    exchange_payload(shared, peer_id);

    /* The BAR0 State write must surface in the shared state table: this
     * peer's entry sits at BAR2 offset `peer_id * 4` inside the first page
     * (F4 layout). The remote-peer observation is part of the cross-peer
     * exchange below. */
    const volatile uint32_t *state_table = (const volatile uint32_t *)shared;
    ivshmem_write_reg32(dev, IVSHMEM_REG_STATE, SMOKE_HANDSHAKE_STATE_SELF);
    if (state_table[peer_id] != SMOKE_HANDSHAKE_STATE_SELF) {
        fail("state", "BAR0 state write did not surface in the state table");
    }
    checkpoint("state");

    result = ivshmem_backend_open(dev, options.backend, &backend);
    if (result != IVSHMEM_OK) {
        fail_err("backend", result);
    }

    if (options.cross_peer) {
        if (peer_id == 0) {
            /* Initiator: self-doorbell coverage first, then the exchange.
             * Its own events are consumed by the self tests, so the reply
             * wait below cannot observe a stale event. */
            self_doorbell_tests(dev, backend, peer_id);
            cross_peer_exchange(shared, peer_id, dev, backend);
        } else {
            /* Responder: the exchange consumes the request event before any
             * self doorbell can merge with it. */
            cross_peer_exchange(shared, peer_id, dev, backend);
            self_doorbell_tests(dev, backend, peer_id);
        }
    } else {
        self_doorbell_tests(dev, backend, peer_id);
    }

    /* Vector 1 is outside the current profile: the doorbell is a no-op and
     * no event may arrive. */
    ivshmem_write_reg32(dev, IVSHMEM_REG_DOORBELL, (peer_id << 16) | 1u);
    int wait_result = ivshmem_backend_wait_event(backend, 200);
    if (wait_result != 0) {
        fail("doorbell", "an unsupported vector produced an event");
    }
    checkpoint("doorbell-unsupported-vector");

    ivshmem_backend_close(backend);
    ivshmem_device_close(dev);

    if (options.cross_peer && peer_id != 0) {
        /* The responder's marker is distinct: the case succeeds only when
         * the initiator observes the completed round trip. */
        printf("ivshmem %s relay pass\n", selected_backend);
        fflush(stdout);
        return 0;
    }

    printf("ivshmem %s pass\n", selected_backend);
    fflush(stdout);
    return 0;
}
