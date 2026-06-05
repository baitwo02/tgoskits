#!/usr/bin/env bash
set -euo pipefail

app_dir="${STARRY_APP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
base_rootfs="${STARRY_ROOTFS:-}"
staging_root="${STARRY_STAGING_ROOT:-}"
overlay_dir="${STARRY_OVERLAY_DIR:-}"
arch="${STARRY_ARCH:-}"
apk_cache="${STARRY_WORKSPACE:-$(cd "$app_dir/../../.." && pwd)}/target/mosquitto-apk-cache"

require_env() {
    local name="$1"
    local value="$2"
    if [[ -z "$value" ]]; then
        echo "error: $name is required" >&2
        exit 1
    fi
}

ensure_host_packages() {
    local missing=()

    command -v debugfs >/dev/null 2>&1 || missing+=(e2fsprogs)
    command -v install >/dev/null 2>&1 || missing+=(coreutils)
    command -v readelf >/dev/null 2>&1 || missing+=(binutils)

    if [[ ${#missing[@]} -eq 0 ]]; then
        return
    fi

    if ! command -v apt-get >/dev/null 2>&1; then
        echo "error: missing required host packages and apt-get is unavailable: ${missing[*]}" >&2
        exit 1
    fi

    echo "installing missing host packages: ${missing[*]}"
    apt-get update
    apt-get install -y --no-install-recommends "${missing[@]}"
}

qemu_runner_candidates() {
    case "$arch" in
        aarch64)     printf '%s\n' qemu-aarch64-static qemu-aarch64 ;;
        riscv64)     printf '%s\n' qemu-riscv64-static qemu-riscv64 ;;
        x86_64)      printf '%s\n' qemu-x86_64-static qemu-x86_64 ;;
        loongarch64) printf '%s\n' qemu-loongarch64-static qemu-loongarch64 ;;
        *)
            echo "error: unsupported Starry arch for Mosquitto prebuild: $arch" >&2
            exit 1
            ;;
    esac
}

find_qemu_runner() {
    local candidate
    while IFS= read -r candidate; do
        if command -v "$candidate" >/dev/null 2>&1; then
            command -v "$candidate"
            return 0
        fi
    done < <(qemu_runner_candidates)

    echo "error: missing qemu-user runner for arch $arch; tried: $(qemu_runner_candidates | paste -sd ', ' -)" >&2
    exit 1
}

run_guest_apk_once() {
    local qemu_runner
    qemu_runner="$(find_qemu_runner)"

    if [[ ! -x "$staging_root/sbin/apk" ]]; then
        echo "error: staging root is missing guest apk: $staging_root/sbin/apk" >&2
        exit 1
    fi

    mkdir -p "$apk_cache"
    QEMU_LD_PREFIX="$staging_root" \
    LD_LIBRARY_PATH="$staging_root/lib:$staging_root/usr/lib" \
        "$qemu_runner" -L "$staging_root" "$staging_root/sbin/apk" \
            --root "$staging_root" \
            --repositories-file "$staging_root/etc/apk/repositories" \
            --keys-dir "$staging_root/etc/apk/keys" \
            --cache-dir "$apk_cache" \
            --update-cache \
            --timeout 60 \
            --no-interactive \
            --force-no-chroot \
            --scripts=no \
            "$@"
}

run_guest_apk() {
    local attempt
    local max_attempts=4

    if [[ -f /etc/resolv.conf ]]; then
        cp /etc/resolv.conf "$staging_root/etc/resolv.conf"
    fi

    for attempt in $(seq 1 "$max_attempts"); do
        if run_guest_apk_once "$@"; then
            return 0
        fi

        if [[ "$attempt" -eq "$max_attempts" ]]; then
            return 1
        fi

        echo "apk command failed, retrying ($attempt/$max_attempts)..." >&2
        sleep $((attempt * 3))
    done
}

extract_base_rootfs() {
    debugfs -R "rdump / $staging_root" "$base_rootfs"
}

install_mosquitto_package() {
    run_guest_apk add mosquitto mosquitto-clients
}

copy_file_to_overlay() {
    local guest_path="$1"
    local mode="$2"
    local source="$staging_root${guest_path}"
    local target="$overlay_dir${guest_path}"

    if [[ ! -e "$source" ]]; then
        echo "error: missing guest file after Mosquitto package install: $guest_path" >&2
        exit 1
    fi

    if [[ -L "$source" ]]; then
        source="$(readlink -f "$source")"
    fi

    install -Dm"$mode" "$source" "$target"
}

find_library_path() {
    local library="$1"
    local dir

    for dir in lib usr/lib usr/local/lib; do
        if [[ -e "$staging_root/$dir/$library" ]]; then
            printf '/%s/%s\n' "$dir" "$library"
            return 0
        fi
    done

    return 1
}

copy_runtime_dependencies() {
    local pending=("$@")
    local seen=" "
    local guest_path library

    while [[ ${#pending[@]} -gt 0 ]]; do
        guest_path="${pending[0]}"
        pending=("${pending[@]:1}")

        if [[ "$seen" == *" $guest_path "* ]]; then
            continue
        fi
        seen+="$guest_path "

        while IFS= read -r library; do
            local library_path
            if ! library_path="$(find_library_path "$library")"; then
                continue
            fi
            copy_file_to_overlay "$library_path" 0644
            pending+=("$library_path")
        done < <(
            readelf -d "$staging_root$guest_path" 2>/dev/null |
                sed -n 's/.*Shared library: \[\(.*\)\].*/\1/p'
        )
    done
}

populate_overlay() {
    copy_file_to_overlay /usr/sbin/mosquitto 0755
    copy_file_to_overlay /usr/bin/mosquitto_pub 0755
    copy_file_to_overlay /usr/bin/mosquitto_sub 0755
    copy_file_to_overlay /usr/bin/mosquitto_passwd 0755
    copy_runtime_dependencies /usr/sbin/mosquitto /usr/bin/mosquitto_pub /usr/bin/mosquitto_sub /usr/bin/mosquitto_passwd

    install -Dm0755 "$app_dir/test_mosquitto.sh" "$overlay_dir/usr/bin/test_mosquitto.sh"
    install -Dm0755 "$app_dir/mosquitto-smoke-tests.sh" "$overlay_dir/usr/bin/mosquitto-smoke-tests.sh"
    install -Dm0755 "$app_dir/mosquitto-tests.sh" "$overlay_dir/usr/bin/mosquitto-tests.sh"
    install -Dm0755 "$app_dir/mosquitto-stress-tests.sh" "$overlay_dir/usr/bin/mosquitto-stress-tests.sh"

    # Create default config directory
    mkdir -p "$overlay_dir/etc/mosquitto"
    cat > "$overlay_dir/etc/mosquitto/mosquitto.conf" << 'EOF'
listener 1883
socket_domain ipv4
allow_anonymous true
persistence true
persistence_location /var/lib/mosquitto/
log_dest stderr
log_type error
log_type warning
log_type notice
log_type information
connection_messages true
EOF
}

require_env STARRY_ROOTFS "$base_rootfs"
require_env STARRY_STAGING_ROOT "$staging_root"
require_env STARRY_OVERLAY_DIR "$overlay_dir"
require_env STARRY_ARCH "$arch"

ensure_host_packages
extract_base_rootfs
install_mosquitto_package
populate_overlay
