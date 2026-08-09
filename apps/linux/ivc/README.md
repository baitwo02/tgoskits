# Axvisor IVC Linux Guest Support

This directory contains the Linux-side user-space pieces used by the Axvisor
IVC QEMU test:

- `include/`: shared ioctl and user library headers.
- `lib/`: small userspace wrapper over the IVC device ioctls.
- `publisher/`: Linux publisher program for Linux-to-ArceOS tests.
- `subscriber/`: Linux subscriber program used by the ArceOS-to-Linux test.

The Linux kernel module that exposes `/dev/axivc` is not kept in tgoskits. It
is built from
[`arceos-hypervisor/axvisor-tools`](https://github.com/arceos-hypervisor/axvisor-tools)
by tgosimages together with the target Linux kernel and installed into the
rootfs as `/root/axvisor.ko`.

## Message V1 demo protocol

Each device `read()` or `write()` transfers one complete, non-empty Message V1
logical message. POSIX zero-length reads cannot distinguish an empty message
from an empty ring, so the Linux read/write adapter rejects empty messages even
though the transport codec can represent them. The kernel module handles cell
fragmentation and reassembly; the programs in this directory define the
application payload:

```text
kind: u8 | sequence: u64 little-endian | body_len: u16 little-endian | body
```

The full-duplex demo sends five ordered Request messages with total lengths
`39, 40, 41, 640, 700`, three independently sequenced Data messages with total
lengths `41, 641, 700`, and one Ack for each Request. These lengths cover a
single cell, the fragment boundary, and messages larger than the ring's
in-flight capacity.

Region v3/Message V1 intentionally rejects the older fixed-slot region v2
layout. The Linux programs and `/root/axvisor.ko` must therefore be updated
together.

## Build and run

Build the test payloads with:

```bash
AXVISOR_IVC_ARCH=aarch64 \
AXVISOR_IVC_OUT_DIR=/path/to/out \
apps/linux/ivc/build.sh
```

The output directory contains:

```text
ivc-publish
ivc-subscribe
```

The command-line forms are:

```text
ivc-publish <channel_key> [channel_size]
ivc-subscribe <publisher_vm_id> <channel_key> [request_count]
```

The Message V1 demo currently requires `request_count` to be five. Run its
host-side application protocol tests with:

```bash
make -C apps/linux/ivc/tests clean test
```

`cargo xtask axvisor test qemu --arch aarch64 --test-group normal --test-case ivc`
builds these payloads as part of the test, injects them into the selected Linux
rootfs, and runs the ArceOS publisher against the Linux subscriber.
