/*
 * ivshmem shared-memory smoke scenario.
 *
 * This program owns only payload, checksum, marker, and timeout diagnostics;
 * every device interaction goes through the libivshmem adapter so the smoke
 * test never hard-codes a BDF, a BAR address, or a sysfs path.
 *
 * Output contract (frozen with the shared QEMU case):
 *   success: "ivshmem polling pass"
 *   failure: "ivshmem polling failed <step>: <detail>"
 *   progress: "ivshmem checkpoint <name>"
 *
 * The polling handshake exercises discovery without a fixed BDF, mapping
 * without absolute addresses, profile registers, a BAR2 payload round-trip,
 * and the doorbell/Event Status path: the peer rings its own doorbell and
 * waits through the polling backend twice (post-clear re-pend), then
 * verifies that an unsupported vector produces no event. Cross-peer payload
 * exchange joins this scenario once the dual-peer case runs again.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ivshmem.h"

/* Payload lives well past the register page in the shared region and away
 * from the state-table area the device owns. */
#define SMOKE_PAYLOAD_OFFSET 0x1000
#define SMOKE_PAYLOAD_SIZE 0x100

static const uint32_t EXPECTED_MAX_PEERS = 2;

static void checkpoint(const char *name)
{
    printf("ivshmem checkpoint %s\n", name);
    fflush(stdout);
}

static void fail(const char *step, const char *detail)
{
    printf("ivshmem polling failed %s: %s\n", step, detail);
    fflush(stdout);
    exit(1);
}

static void fail_err(const char *step, int err)
{
    fail(step, ivshmem_strerror(err));
}

static void usage(const char *program)
{
    fprintf(stderr, "usage: %s --backend polling [--bdf <BDF>]\n", program);
}

struct options {
    const char *bdf;
};

static void parse_options(int argc, char **argv, struct options *options)
{
    int index;

    memset(options, 0, sizeof(*options));
    for (index = 1; index < argc; index++) {
        if (strcmp(argv[index], "--backend") == 0 && index + 1 < argc) {
            index++;
            if (strcmp(argv[index], "polling") != 0) {
                /* The interrupt backend exists only with F7; there is no
                 * silent fallback to polling. */
                fail("backend", "only the polling backend exists in this "
                                "adapter revision");
            }
        } else if (strcmp(argv[index], "--bdf") == 0 && index + 1 < argc) {
            index++;
            options->bdf = argv[index];
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

static void exchange_payload(void *shared)
{
    uint8_t payload[SMOKE_PAYLOAD_SIZE];
    volatile uint8_t *remote = (volatile uint8_t *)shared + SMOKE_PAYLOAD_OFFSET;
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
    checkpoint("payload");
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
    int wait_result;

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
    exchange_payload(shared);

    /* The BAR0 State write must surface in the shared state table: this
     * peer's entry sits at BAR2 offset `peer_id * 4` inside the first page
     * (F4 layout). The remote-peer observation joins the case together with
     * the dual-peer run. */
    const volatile uint32_t *state_table = (const volatile uint32_t *)shared;
    ivshmem_write_reg32(dev, IVSHMEM_REG_STATE, 0x00010002u);
    if (state_table[peer_id] != 0x00010002u) {
        fail("state", "BAR0 state write did not surface in the state table");
    }
    checkpoint("state");

    result = ivshmem_backend_open(dev, IVSHMEM_BACKEND_POLLING, &backend);
    if (result != IVSHMEM_OK) {
        fail_err("backend", result);
    }

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

    /* Vector 1 is outside the current profile: the doorbell is a no-op and
     * no event may arrive. */
    ivshmem_write_reg32(dev, IVSHMEM_REG_DOORBELL, (peer_id << 16) | 1u);
    wait_result = ivshmem_backend_wait_event(backend, 200);
    if (wait_result != 0) {
        fail("doorbell", "an unsupported vector produced an event");
    }
    checkpoint("doorbell-unsupported-vector");

    ivshmem_backend_close(backend);
    ivshmem_device_close(dev);

    printf("ivshmem polling pass\n");
    fflush(stdout);
    return 0;
}
