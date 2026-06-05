#!/usr/bin/env bash
set -euo pipefail

app_dir="${STARRY_APP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
overlay_dir="${STARRY_OVERLAY_DIR:-}"
base_rootfs="${STARRY_ROOTFS:-}"

if [[ -z "$overlay_dir" ]]; then
    echo "error: STARRY_OVERLAY_DIR is required" >&2
    exit 1
fi
if [[ -z "$base_rootfs" ]]; then
    echo "error: STARRY_ROOTFS is required" >&2
    exit 1
fi

case "$STARRY_ARCH" in
    aarch64)
        MUSL_TARGET="aarch64-linux-musl"
        MUSL_ARCH="aarch64"
        ;;
    riscv64)
        MUSL_TARGET="riscv64-linux-musl"
        MUSL_ARCH="riscv64"
        ;;
    x86_64)
        MUSL_TARGET="x86_64-linux-musl"
        MUSL_ARCH="x86_64"
        ;;
    *)
        echo "ERROR: unsupported arch: $STARRY_ARCH" >&2
        exit 1
        ;;
esac

command -v debugfs >/dev/null 2>&1 || { echo "ERROR: debugfs not found" >&2; exit 1; }
command -v readelf >/dev/null 2>&1 || { echo "ERROR: readelf not found" >&2; exit 1; }

qemu_runner() {
    case "$STARRY_ARCH" in
        aarch64) printf "%s\n" qemu-aarch64-static ;;
        riscv64) printf "%s\n" qemu-riscv64-static ;;
        x86_64) printf "%s\n" qemu-x86_64-static ;;
        *) echo "ERROR: unsupported arch for qemu-user: $STARRY_ARCH" >&2; return 1 ;;
    esac
}

install_lld_in_sysroot() {
    if [[ -x "$sysroot/usr/bin/ld.lld" ]]; then
        return
    fi
    lld_installed_in_prebuild=1

    local runner
    runner="$(qemu_runner)"
    command -v "$runner" >/dev/null 2>&1 || { echo "ERROR: $runner not found; cannot install lld into rootfs" >&2; exit 1; }
    [[ -x "$sysroot/sbin/apk" ]] || { echo "ERROR: rootfs is missing /sbin/apk; cannot install lld" >&2; exit 1; }

    if [[ -f /etc/resolv.conf ]]; then
        cp /etc/resolv.conf "$sysroot/etc/resolv.conf"
    fi

    local apk_cache="${STARRY_WORKSPACE:-$(cd "$app_dir/../../.." && pwd)}/target/musl-dynamic-smoke-apk-cache"
    mkdir -p "$apk_cache"
    echo "installing lld into $STARRY_ARCH rootfs staging..."
    QEMU_LD_PREFIX="$sysroot" \
    LD_LIBRARY_PATH="$sysroot/lib:$sysroot/usr/lib" \
        "$runner" -L "$sysroot" "$sysroot/sbin/apk" \
            --root "$sysroot" \
            --repositories-file "$sysroot/etc/apk/repositories" \
            --keys-dir "$sysroot/etc/apk/keys" \
            --cache-dir "$apk_cache" \
            --update-cache \
            --timeout 60 \
            --no-interactive \
            --force-no-chroot \
            --scripts=no \
            add lld
}

copy_rootfs_file_to_overlay() {
    local guest_path="$1"
    local mode="$2"
    local source="$sysroot$guest_path"
    local target="$overlay_dir$guest_path"

    [[ -e "$source" ]] || { echo "ERROR: missing rootfs file after lld install: $guest_path" >&2; exit 1; }
    if [[ -L "$source" ]]; then
        source="$(readlink -f "$source")"
    fi
    install -Dm"$mode" "$source" "$target"
}

find_rootfs_library_path() {
    local library="$1"
    local dir
    for dir in lib usr/lib usr/local/lib; do
        if [[ -e "$sysroot/$dir/$library" ]]; then
            printf "/%s/%s\n" "$dir" "$library"
            return 0
        fi
    done
    return 1
}

copy_lld_runtime_to_overlay() {
    local pending=(/usr/bin/ld.lld /usr/bin/lld)
    local seen=" "
    local guest_path library library_path

    while [[ ${#pending[@]} -gt 0 ]]; do
        guest_path="${pending[0]}"
        pending=("${pending[@]:1}")
        if [[ "$seen" == *" $guest_path "* ]]; then
            continue
        fi
        seen+="$guest_path "

        copy_rootfs_file_to_overlay "$guest_path" 0755
        while IFS= read -r library; do
            if library_path="$(find_rootfs_library_path "$library")"; then
                pending+=("$library_path")
            fi
        done < <(readelf -d "$sysroot$guest_path" 2>/dev/null | sed -n 's/.*Shared library: \[\(.*\)\].*/\1/p')
    done
}

# Resolve an ELF lld driver on PATH. clang/GCC use `-fuse-ld=lld`,
# which looks for an `ld.lld`-style driver, so wrap generic `lld` or
# Rust toolchain `rust-lld` with `-flavor gnu` when needed. This avoids
# falling back to musl-cross GCC's default GNU ld, which cannot consume
# the .relr.dyn section shipped in current Alpine libc.so.
lld_linker=""
lld_linker_dir=""
lld_installed_in_prebuild=""
if command -v ld.lld >/dev/null 2>&1; then
    lld_linker="$(command -v ld.lld)"
elif command -v lld >/dev/null 2>&1; then
    lld_linker_dir="$(mktemp -d)"
    printf '#!/usr/bin/env bash\nexec %q -flavor gnu "$@"\n' \
        "$(command -v lld)" >"$lld_linker_dir/ld.lld"
    chmod +x "$lld_linker_dir/ld.lld"
    lld_linker="$lld_linker_dir/ld.lld"
elif command -v rust-lld >/dev/null 2>&1; then
    lld_linker_dir="$(mktemp -d)"
    printf '#!/usr/bin/env bash\nexec %q -flavor gnu "$@"\n' \
        "$(command -v rust-lld)" >"$lld_linker_dir/ld.lld"
    chmod +x "$lld_linker_dir/ld.lld"
    lld_linker="$lld_linker_dir/ld.lld"
fi
if [[ -n "$lld_linker_dir" ]]; then
    PATH="$lld_linker_dir:$PATH"
fi

# Build into a temp directory so the compiled ELF never lands inside the
# application source tree. The trap cleans both sysroot and build_dir on exit.
sysroot="$(mktemp -d)"
build_dir="$(mktemp -d)"
trap 'rm -rf "$sysroot" "$build_dir" "$lld_linker_dir"' EXIT
debugfs -R "rdump / $sysroot" "$base_rootfs" >/dev/null 2>&1
install_lld_in_sysroot
if [[ -n "$lld_installed_in_prebuild" ]]; then
    copy_lld_runtime_to_overlay
    copy_rootfs_file_to_overlay /lib/apk/db/installed 0644
fi

if command -v clang >/dev/null 2>&1 && [[ -n "$lld_linker" ]]; then
    CC="clang"
    CC_FLAGS="--target=$MUSL_TARGET --sysroot=$sysroot -isystem $sysroot/usr/include -fuse-ld=lld -nostdlib -Wl,--strip-debug"
elif command -v "${MUSL_TARGET}-gcc" >/dev/null 2>&1; then
    CC="${MUSL_TARGET}-gcc"
    if [[ -n "$lld_linker" ]]; then
        # -nostdlib disables the toolchain's default CRT (crt1.o/crti.o/...)
        # so the script can supply the Alpine musl Scrt1.o/crti.o/crtn.o
        # trio by hand without producing _start/_init/_fini duplicates.
        # -fuse-ld=lld routes the link through the resolved lld driver,
        # which (unlike GNU ld) handles the .relr.dyn section in current
        # Alpine libc.so.
        CC_FLAGS="--sysroot=$sysroot -nostdlib -fuse-ld=lld"
    else
        echo "ERROR: ${MUSL_TARGET}-gcc found, but no lld driver (lld, ld.lld, or rust-lld) on PATH." >&2
        echo "  The Alpine rootfs in this smoke case uses .relr.dyn, which GNU ld cannot consume." >&2
        echo "  Install lld/lld.lld (e.g. 'apt-get install lld-14') or a Rust toolchain with rust-lld and retry." >&2
        exit 1
    fi
else
    echo "ERROR: no compiler for $MUSL_TARGET (tried clang+lld/ld.lld/rust-lld, ${MUSL_TARGET}-gcc)" >&2
    exit 1
fi

$CC $CC_FLAGS \
    -L"$sysroot/usr/lib" \
    -Wl,--library-path="$sysroot/usr/lib" \
    "$sysroot/usr/lib/Scrt1.o" \
    "$sysroot/usr/lib/crti.o" \
    -lc \
    "$sysroot/usr/lib/crtn.o" \
    -o "$build_dir/dynamic-test" \
    "$app_dir/dynamic-test.c"

INTERP=$(readelf -l "$build_dir/dynamic-test" | sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')
echo "INTERP path: $INTERP"
[[ -n "$INTERP" ]] || { echo "ERROR: no PT_INTERP found" >&2; exit 1; }

INTERP_BASENAME=$(basename "$INTERP")
MUSL_LD="$sysroot/lib/$INTERP_BASENAME"
if [[ ! -f "$MUSL_LD" ]]; then
    MUSL_LD="$sysroot/lib/libc.musl-$MUSL_ARCH.so.1"
fi
[[ -f "$MUSL_LD" ]] || { echo "ERROR: musl ld not found" >&2; exit 1; }

install -Dm0755 "$build_dir/dynamic-test" "$overlay_dir/usr/bin/dynamic-test"
install -Dm0755 "$MUSL_LD" "$overlay_dir/$INTERP"
install -Dm0755 "$app_dir/dynamic-test.sh" "$overlay_dir/usr/bin/dynamic-test.sh"
