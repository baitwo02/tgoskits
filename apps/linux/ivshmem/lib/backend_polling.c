/*
 * BAR mapping, register access, and the polling event backend.
 *
 * All addressing is BAR-relative: the adapter reads each BAR's size from the
 * sysfs `resource` table and maps the corresponding `resourceN` file, so no
 * caller ever stores a hypervisor-assigned absolute address. The polling
 * backend owns the W1C handshake on Event Status so callers cannot get the
 * clear-then-recheck order wrong.
 */
#define _POSIX_C_SOURCE 200809L

#include "ivshmem_internal.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

/* Milliseconds between Event Status polls; short enough for smoke timeouts,
 * slow enough not to saturate a single guest CPU. */
#define IVSHMEM_POLL_INTERVAL_MS 1

struct ivshmem_backend {
    struct ivshmem_device *dev;
};

int ivshmem_enable_device(struct ivshmem_device *dev)
{
    char path[IVSHMEM_PATH_MAX];
    uint16_t command = 0;
    ssize_t transferred;
    int fd;

    if (dev == NULL) {
        return IVSHMEM_ERR_ARGS;
    }
    snprintf(path, sizeof(path), "%s/config", dev->sysfs_dir);
    fd = open(path, O_RDWR);
    if (fd < 0) {
        return IVSHMEM_ERR_IO;
    }
    transferred = pread(fd, &command, sizeof(command),
                        IVSHMEM_CONFIG_COMMAND_OFFSET);
    if (transferred != (ssize_t)sizeof(command)) {
        close(fd);
        return IVSHMEM_ERR_IO;
    }
    command |= IVSHMEM_COMMAND_MEMORY_ENABLE;
    transferred = pwrite(fd, &command, sizeof(command),
                         IVSHMEM_CONFIG_COMMAND_OFFSET);
    if (transferred != (ssize_t)sizeof(command)) {
        close(fd);
        return IVSHMEM_ERR_IO;
    }
    /* Verify the sticky write so a later BAR access failure cannot hide a
     * silently ignored enable. */
    command = 0;
    transferred = pread(fd, &command, sizeof(command),
                        IVSHMEM_CONFIG_COMMAND_OFFSET);
    close(fd);
    if (transferred != (ssize_t)sizeof(command) ||
        (command & IVSHMEM_COMMAND_MEMORY_ENABLE) == 0) {
        return IVSHMEM_ERR_IO;
    }
    return IVSHMEM_OK;
}

static int is_mappable_bar(uint8_t bar)
{
    return bar == IVSHMEM_BAR_REGISTERS || bar == IVSHMEM_BAR_SHARED;
}

static const char *bar_resource_name(uint8_t bar)
{
    static __thread char name[32];

    snprintf(name, sizeof(name), "resource%u", (unsigned)bar);
    return name;
}

int ivshmem_bar_size(const struct ivshmem_device *dev, uint8_t bar,
                     size_t *size)
{
    char path[IVSHMEM_PATH_MAX];
    FILE *file;
    unsigned long start = 0;
    unsigned long end = 0;
    unsigned long flags = 0;
    unsigned long line_start = 0;
    unsigned long line_end = 0;
    unsigned long line_flags = 0;
    unsigned long index;

    if (dev == NULL || size == NULL || bar >= IVSHMEM_MAX_BARS) {
        return IVSHMEM_ERR_ARGS;
    }
    snprintf(path, sizeof(path), "%s/resource", dev->sysfs_dir);
    file = fopen(path, "r");
    if (file == NULL) {
        return IVSHMEM_ERR_IO;
    }
    for (index = 0; index <= bar; index++) {
        if (fscanf(file, "%lx %lx %lx", &line_start, &line_end,
                   &line_flags) != 3) {
            fclose(file);
            return IVSHMEM_ERR_IO;
        }
        start = line_start;
        end = line_end;
        flags = line_flags;
    }
    fclose(file);

    if (start == 0 && end == 0) {
        /* The BAR exists in the profile but was never assigned. */
        return IVSHMEM_ERR_IO;
    }
    if ((flags & IVSHMEM_RESOURCE_MEM_FLAG) == 0) {
        /* Only memory BARs map into the guest as shared windows. */
        return IVSHMEM_ERR_ARGS;
    }
    if (end < start) {
        return IVSHMEM_ERR_IO;
    }
    *size = (size_t)(end - start) + 1;
    return IVSHMEM_OK;
}

int ivshmem_map_bar(struct ivshmem_device *dev, uint8_t bar, void **map,
                    size_t *size)
{
    char path[IVSHMEM_PATH_MAX];
    struct ivshmem_mapping *mapping;
    size_t bar_bytes = 0;
    int result;

    if (dev == NULL || map == NULL || size == NULL) {
        return IVSHMEM_ERR_ARGS;
    }
    if (bar == IVSHMEM_BAR_MSIX) {
        /* The MSI-X BAR has no guest-visible semantics before F7. */
        return IVSHMEM_ERR_BACKEND;
    }
    if (!is_mappable_bar(bar)) {
        return IVSHMEM_ERR_ARGS;
    }
    mapping = &dev->bars[bar];
    if (mapping->map != NULL) {
        *map = mapping->map;
        *size = mapping->size;
        return IVSHMEM_OK;
    }

    result = ivshmem_bar_size(dev, bar, &bar_bytes);
    if (result != IVSHMEM_OK) {
        return result;
    }
    snprintf(path, sizeof(path), "%s/%s", dev->sysfs_dir,
             bar_resource_name(bar));
    mapping->fd = open(path, O_RDWR | O_SYNC);
    if (mapping->fd < 0) {
        return IVSHMEM_ERR_MMAP;
    }
    mapping->map = mmap(NULL, bar_bytes, PROT_READ | PROT_WRITE, MAP_SHARED,
                        mapping->fd, 0);
    if (mapping->map == MAP_FAILED) {
        close(mapping->fd);
        mapping->fd = -1;
        return IVSHMEM_ERR_MMAP;
    }
    mapping->size = bar_bytes;
    *map = mapping->map;
    *size = mapping->size;
    return IVSHMEM_OK;
}

static const struct ivshmem_mapping *register_mapping(
    const struct ivshmem_device *dev)
{
    if (dev == NULL) {
        return NULL;
    }
    if (dev->bars[IVSHMEM_BAR_REGISTERS].map == NULL) {
        return NULL;
    }
    return &dev->bars[IVSHMEM_BAR_REGISTERS];
}

uint32_t ivshmem_read_reg32(const struct ivshmem_device *dev, uint32_t offset)
{
    const struct ivshmem_mapping *mapping = register_mapping(dev);
    volatile const uint32_t *register_value;

    if (mapping == NULL || offset % sizeof(uint32_t) != 0 ||
        offset >= IVSHMEM_REG_PAGE_SIZE) {
        fprintf(stderr,
                "ivshmem: rejecting unaligned or unmapped BAR0 read at "
                "0x%08x\n",
                offset);
        return 0;
    }
    register_value = (volatile const uint32_t *)((const char *)mapping->map +
                                                 offset);
    return *register_value;
}

void ivshmem_write_reg32(const struct ivshmem_device *dev, uint32_t offset,
                         uint32_t value)
{
    const struct ivshmem_mapping *mapping = register_mapping(dev);
    volatile uint32_t *register_value;

    if (mapping == NULL || offset % sizeof(uint32_t) != 0 ||
        offset >= IVSHMEM_REG_PAGE_SIZE) {
        fprintf(stderr,
                "ivshmem: rejecting unaligned or unmapped BAR0 write at "
                "0x%08x\n",
                offset);
        return;
    }
    register_value =
        (volatile uint32_t *)((char *)mapping->map + offset);
    *register_value = value;
}

void *ivshmem_shared_memory(const struct ivshmem_device *dev, size_t *size)
{
    if (dev == NULL) {
        return NULL;
    }
    if (dev->bars[IVSHMEM_BAR_SHARED].map == NULL) {
        if (size != NULL) {
            *size = 0;
        }
        return NULL;
    }
    if (size != NULL) {
        *size = dev->bars[IVSHMEM_BAR_SHARED].size;
    }
    return dev->bars[IVSHMEM_BAR_SHARED].map;
}

int ivshmem_backend_open(struct ivshmem_device *dev,
                         enum ivshmem_backend_kind kind,
                         struct ivshmem_backend **out)
{
    struct ivshmem_backend *backend;

    if (dev == NULL || out == NULL) {
        return IVSHMEM_ERR_ARGS;
    }
    if (kind != IVSHMEM_BACKEND_POLLING) {
        /* Explicit refusal, never a silent polling fallback. */
        return IVSHMEM_ERR_BACKEND;
    }
    backend = calloc(1, sizeof(*backend));
    if (backend == NULL) {
        return IVSHMEM_ERR_NOMEM;
    }
    backend->dev = dev;
    *out = backend;
    return IVSHMEM_OK;
}

static long monotonic_ms(void)
{
    struct timespec now;

    clock_gettime(CLOCK_MONOTONIC, &now);
    return now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

static void sleep_poll_interval(void)
{
    struct timespec pause = { .tv_sec = 0,
                              .tv_nsec = IVSHMEM_POLL_INTERVAL_MS * 1000000 };

    nanosleep(&pause, NULL);
}

int ivshmem_backend_wait_event(struct ivshmem_backend *be, int timeout_ms)
{
    long deadline = timeout_ms < 0 ? 0 : monotonic_ms() + timeout_ms;

    if (be == NULL || be->dev == NULL) {
        return IVSHMEM_ERR_ARGS;
    }
    for (;;) {
        uint32_t status = ivshmem_read_reg32(be->dev,
                                             IVSHMEM_REG_EVENT_STATUS);

        if (status & 1) {
            /* The backend owns the W1C clear so callers cannot forget it;
             * the next call re-reads Event Status and protocol state, per
             * the release-order contract frozen with the doorbell feature. */
            ivshmem_write_reg32(be->dev, IVSHMEM_REG_EVENT_STATUS, 1);
            return 1;
        }
        if (timeout_ms >= 0 && monotonic_ms() >= deadline) {
            return 0;
        }
        sleep_poll_interval();
    }
}

void ivshmem_backend_close(struct ivshmem_backend *be)
{
    free(be);
}
