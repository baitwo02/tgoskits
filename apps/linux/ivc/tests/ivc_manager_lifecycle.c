#include <ivc/ioctl_args.h>
#include <ivc/ulib.h>

#include <errno.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

static const int manager_fd = 7;
static const int publisher_fd = 42;
static int open_requests;
static int close_requests;
static int cleanup_requests;
static int free_requests;
static int close_result;
static void *manager_allocation;
static void *publisher_allocation;
static int unexpected_call;

int __wrap_open(const char *path, int flags, ...)
{
    (void)path;
    (void)flags;
    open_requests++;
    if (open_requests == 1) {
        return manager_fd;
    }
    if (open_requests == 2) {
        return publisher_fd;
    }
    unexpected_call = 1;
    errno = EIO;
    return -1;
}

int __wrap_close(int fd)
{
    close_requests++;
    if ((close_requests == 1 && fd != publisher_fd) ||
        (close_requests == 2 && fd != manager_fd)) {
        unexpected_call = 1;
    }
    if (close_result < 0) {
        errno = EIO;
    }
    return close_result;
}

int __wrap_ioctl(int fd, unsigned long request, ...)
{
    va_list args;
    void *arg;

    va_start(args, request);
    arg = va_arg(args, void *);
    va_end(args);

    if (request == IVC_PUBLISH_CHANNEL && fd == manager_fd) {
        ivc_publish_arg_t *publish_arg = arg;
        strcpy(publish_arg->device_name, "/dev/mock-ivc-publisher");
        return 0;
    }
    if (request == IVC_UNPUBLISH_CHANNEL && fd == manager_fd) {
        cleanup_requests++;
        return 0;
    }

    unexpected_call = 1;
    errno = EIO;
    return -1;
}

void __wrap_free(void *ptr)
{
    free_requests++;
    if (ptr != publisher_allocation && ptr != manager_allocation) {
        unexpected_call = 1;
    }
}

int main(void)
{
    ivc_manager_p manager = ivc_open_manager();
    if (!manager) {
        fprintf(stderr, "manager open unexpectedly failed\n");
        return 1;
    }
    manager_allocation = manager;

    ivc_publisher_p publisher = ivc_publish(manager, 0x100, 4096);
    if (!publisher) {
        fprintf(stderr, "publisher creation unexpectedly failed\n");
        return 1;
    }
    publisher_allocation = publisher;

    errno = 0;
    if (ivc_close_manager(manager) != -1 || errno != EBUSY) {
        fprintf(stderr, "manager close did not reject a live endpoint\n");
        return 1;
    }
    if (close_requests != 0 || free_requests != 0) {
        fprintf(stderr, "busy manager close consumed a live manager\n");
        return 1;
    }

    if (ivc_unpublish(publisher) != 0) {
        fprintf(stderr, "publisher teardown unexpectedly failed\n");
        return 1;
    }
    if (close_requests != 1 || cleanup_requests != 1 || free_requests != 1) {
        fprintf(stderr, "publisher teardown did not release all resources\n");
        return 1;
    }

    close_result = -1;
    if (ivc_close_manager(manager) != -1) {
        fprintf(stderr, "manager close failure was not reported\n");
        return 1;
    }
    if (close_requests != 2 || free_requests != 2) {
        fprintf(stderr, "manager allocation leaked after close failure\n");
        return 1;
    }
    if (unexpected_call != 0) {
        fprintf(stderr, "manager lifecycle used an unexpected operation\n");
        return 1;
    }

    return 0;
}
