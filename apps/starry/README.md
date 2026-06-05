# Starry Apps

`apps/starry/` contains runnable StarryOS scenarios. Most direct child
directories are board cases selected by `cargo xtask starry app board -t <case>`;
some x86_64 QEMU demos provide their own `cargo xtask starry qemu` commands.

Cases are intentionally separate from `test-suit/starryos`: apps are
operator-facing workflows, while the test suit remains CI-oriented coverage.

## Case Layout

```text
apps/starry/<case>/
  init.sh
  build-<target>.toml
  board-<board>.toml
  <optional user projects>
```

- `init.sh` is read by `cargo xtask starry app board` and sent to the Starry shell
  as the startup command.
- `build-<target>.toml` is the StarryOS build config. It must either include a
  top-level `target = "..."` or encode the target in the filename.
- `board-<board>.toml` is the ostool board run config. It supplies the board
  type, shell prefix, success/failure regexes, timeout, and optional server
  defaults.
- User programs under the case are examples only. The board rootfs must already
  contain the program and its shared libraries unless the case says otherwise.

### Build config notes

For QEMU app cases, keep `plat_dyn = true` in `build-aarch64-unknown-none-softfloat.toml`
and `build-riscv64gc-unknown-none-elf.toml` unless the case deliberately targets a
registered static platform. Starry has no static default platform for aarch64 or
riscv64, so setting `plat_dyn = false` for those generic QEMU targets makes the
build fail with `no default platform package is registered for arch ...`.

Static `plat_dyn = false` configs are valid for arches with a Starry static
default, such as x86_64 and loongarch64, or for board/platform-specific cases
that explicitly select the platform they need.

### QEMU rootfs notes

QEMU app cases must boot with a guest rootfs image that matches the app
configuration. Standard rootfs image names are fixed: use
`tmp/axbuild/rootfs/rootfs-<arch>-alpine.img` for Alpine and
`tmp/axbuild/rootfs/rootfs-<arch>-debian.img` for Debian. Do not rename these
paths casually; `qemu-<arch>.toml`, prebuild scripts, and overlay injection flows
treat them as part of the app-runner contract. Prepare the standard image with
`cargo xtask starry rootfs --arch <arch>` before running a case, unless the case
README or QEMU config explicitly names a case-specific image.

If a case uses `prebuild.sh` or an overlay, the base image still has to be the
expected distro and architecture because the app runner extracts files from it,
installs packages into a staging root, and injects the result back into the QEMU
rootfs. Keep `-drive ... rootfs-<arch>-alpine.img` and any kernel `root=...`
argument consistent with the selected image and bus.

Example:

```bash
cargo xtask starry app board -t orangepi-5-plus-uvc
```

## PicoClaw CLI

The `picoclaw-cli` case is an opt-in StarryOS x86_64 QEMU workflow for checking
PicoClaw compatibility in three stages: offline CLI smoke, online agent request,
and gateway service smoke. It also provides an interactive StarryOS shell for
manual PicoClaw use. It prepares local-only release assets and rootfs images
under `target/picoclaw/` and `tmp/axbuild/rootfs/`.

```bash
apps/starry/picoclaw-cli/prepare_picoclaw_rootfs.sh
cargo xtask starry qemu \
  --arch x86_64 \
  --qemu-config apps/starry/picoclaw-cli/qemu-x86_64-picoclaw-offline.toml \
  --rootfs tmp/axbuild/rootfs/rootfs-x86_64-picoclaw.img
```

See `picoclaw-cli/README.md` for the online agent, gateway, and interactive
flows.

## K230 KPU NNCase

The `k230-kpu-nncase` case is the operator-facing K230 KPU/NPU demo. It installs
the StarryOS guest NNCase runtime demo binaries, `yolov8n_320.kmodel`, and
`bus.jpg` into the K230 rootfs overlay, then runs:

```text
.kmodel -> NNCase runtime -> KPU command stream -> /dev/kpu -> IRQ/done -> output hashes
```

```bash
bash apps/starry/k230-kpu-nncase/c/tools/build-nncase-runtime-binaries.sh
PATH="$PWD/target/qemu-k230-docker-build:$PATH" \
  cargo xtask starry app qemu -t k230-kpu-nncase --arch riscv64
```

See `k230-kpu-nncase/README.md` and `docs/k230-kpu-nncase-runtime.md` for the
asset preparation flow.

## Redis

The `redis` case is a QEMU app workflow that installs Redis into a temporary
Alpine staging root and injects the Redis binaries, scripts, and runtime
libraries into the app rootfs overlay.

```bash
cargo xtask starry app qemu -t redis --arch riscv64
```

Stress configs are available through explicit QEMU config variants; see
`redis/README.md`.

## GDB Smoke

The `gdb-smoke` case is a RISC-V QEMU app workflow that prepares a temporary
rootfs overlay with GDB, GDBServer, and two tiny target programs.

```bash
cargo xtask starry app qemu -t gdb-smoke --arch riscv64
cargo xtask starry app qemu -t gdb-smoke --arch riscv64 \
  --qemu-config qemu-riscv64-gdbserver.toml
```

## MariaDB

The `mariadb` case is a QEMU app workflow that installs MariaDB in the guest,
initializes a fresh data directory, runs an InnoDB SQL workload, and checks that
the data survives a server restart.

```bash
cargo xtask starry app qemu -t mariadb --arch aarch64
cargo xtask starry app qemu -t mariadb --arch loongarch64
cargo xtask starry app qemu -t mariadb --arch x86_64
cargo xtask starry app qemu -t mariadb --arch riscv64
```

## jcode

The `jcode` case is an x86_64 QEMU app workflow that downloads the jcode AI coding
agent from GitHub releases, patches the glibc-linked binary for musl compatibility
using `patchelf`, builds a glibc stub shared library, and injects everything into
the app rootfs overlay.

```bash
apps/starry/jcode/prepare_jcode_rootfs.sh
cargo xtask starry qemu \
  --arch x86_64 \
  --qemu-config apps/starry/jcode/qemu-x86_64.toml \
  --rootfs tmp/axbuild/rootfs/rootfs-x86_64-jcode.img
```

See `jcode/README.md` for interactive usage and troubleshooting.

## Nginx

The `nginx` case is a QEMU app integration workflow. It installs Alpine nginx
packages in a staging root during prebuild, injects runtime artifacts to the
app overlay, then runs nginx smoke tests inside StarryOS.

```bash
cargo xtask starry app qemu -t nginx --arch x86_64
```

`apps/starry/nginx` maintains four directories: `smoke`, `phase`, `stress`, and
`debug`. Currently only smoke is connected as nginx test entry in tgoskits workflows.

## Orange Pi 5 Plus UVC

The `orangepi-5-plus-uvc` case needs `/usr/bin/uvc-fps` to be installed in the
board rootfs before StarryOS is booted. The usual preparation flow is:

1. reserve the board with `cargo board connect --board-type OrangePi-5-Plus`
   and leave that serial session open;
2. boot into the board Linux shell and read the board IP from the login banner
   or `ip -br addr`;
3. use SSH from the host to copy `apps/starry/orangepi-5-plus-uvc/uvc-fps/`
   into the board Linux system;
4. build and install `uvc-fps` on the board Linux rootfs;
5. close the `cargo board connect` session, then boot StarryOS with:

```bash
cargo xtask starry app board -t orangepi-5-plus-uvc
```

See `orangepi-5-plus-uvc/README.md` for the complete copy, build, install, and
test commands.
