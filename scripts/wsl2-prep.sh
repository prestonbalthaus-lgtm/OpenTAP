#!/usr/bin/env bash
# OpenTAP: prepare a WSL 2 distribution (Debian 13) for the first Linux run.
#
# Runs INSIDE the distribution. It never touches the Windows side beyond
# copying a kernel image to /mnt/e/WSL/kernel, never reboots, never runs
# `wsl --shutdown` and never changes firmware settings: what Windows must do
# is printed for the operator.
#
#     scripts/wsl2-prep.sh [--probe]               a pass/fail table (the default)
#     sudo scripts/wsl2-prep.sh --install-deps     the Debian packages the build,
#                                                  the kernel build and the tests need
#     scripts/wsl2-prep.sh --build-kernel [TAG]    Microsoft's WSL kernel with ublk and
#                                                  dm-log-writes as modules, copied to
#                                                  E:\WSL\kernel\bzImage
#
# Options for --build-kernel: --work DIR (an ext4 directory; default
# ~/wsl-kernel), --jobs N (default 6: the VM has 8 processors, leave
# headroom), --kernel-out DIR (default /mnt/e/WSL/kernel). The default tag
# is linux-msft-wsl-6.18.54.1 from github.com/microsoft/WSL2-Linux-Kernel.
# Microsoft's config-wsl has `# CONFIG_BLK_DEV_UBLK is not set` and
# `# CONFIG_DM_LOG_WRITES is not set`; OpenTAP needs ublk (the 6.12 UAPI:
# ublk, io_uring with IORING_REGISTER_CLONE_BUFFERS), so any kernel 6.12 or
# newer built this way will do. Every step is idempotent: run it again and
# it reuses the tarball, the tree and the objects it finds.
#
# The order of the first day (see docs/step6/notes/6c-transport.md):
#   Windows: BIOS "SVM Mode" on, reboot; `wsl --install --no-distribution`;
#            install Debian (move its VHDX to E: if wanted); in Debian:
#   sudo scripts/wsl2-prep.sh --install-deps     (dist-upgrades a bookworm image first
#                                                 only with --dist-upgrade)
#   scripts/wsl2-prep.sh --build-kernel          then the printed .wslconfig line and
#                                                 `wsl --shutdown` on Windows
#   sudo cp -a <staged modules> /lib/modules/ && sudo depmod -a   (printed)
#   scripts/wsl2-prep.sh --probe                 every line PASS
#   sudo scripts/linux-smoke.sh
set -euo pipefail

DEFAULT_TAG="linux-msft-wsl-6.18.54.1"
REPO_URL="https://github.com/microsoft/WSL2-Linux-Kernel"
WIN_REPO="/mnt/e/Coding/OpenTAP"
MIN_MAJOR=6
MIN_MINOR=12

MODE="probe"
TAG="$DEFAULT_TAG"
WORK=""
JOBS=6
KERNEL_OUT="/mnt/e/WSL/kernel"
DIST_UPGRADE=0

usage() { sed -n '2,35p' "$0"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --probe) MODE="probe"; shift ;;
        --install-deps) MODE="deps"; shift ;;
        --build-kernel)
            MODE="kernel"; shift
            if [ $# -gt 0 ] && [ "${1#--}" = "$1" ]; then TAG="$1"; shift; fi ;;
        --work) WORK="$2"; shift 2 ;;
        --jobs) JOBS="$2"; shift 2 ;;
        --kernel-out) KERNEL_OUT="$2"; shift 2 ;;
        --dist-upgrade) DIST_UPGRADE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option $1 (see --help)" >&2; exit 2 ;;
    esac
done

is_root() { [ "$(id -u)" -eq 0 ]; }

# The user a build runs as: the one who called sudo, else ourselves.
build_user() { echo "${SUDO_USER:-$(id -un)}"; }

# --- the probe ---------------------------------------------------------------

FAILS=0
WARNS=0
ROWS=()

row() {
    # row RESULT CHECK DETAIL
    ROWS+=("$(printf '%-5s  %-26s %s' "$1" "$2" "$3")")
    case "$1" in
        FAIL) FAILS=$((FAILS + 1)) ;;
        WARN) WARNS=$((WARNS + 1)) ;;
    esac
}

probe_kernel() {
    local rel major minor
    rel="$(uname -r)"
    major="$(echo "$rel" | cut -d. -f1)"
    minor="$(echo "$rel" | cut -d. -f2 | sed 's/[^0-9].*//')"
    major="${major:-0}"
    minor="${minor:-0}"
    if [ "$major" -gt "$MIN_MAJOR" ] || { [ "$major" -eq "$MIN_MAJOR" ] && [ "$minor" -ge "$MIN_MINOR" ]; }; then
        row PASS "kernel >= ${MIN_MAJOR}.${MIN_MINOR}" "$rel"
    else
        row FAIL "kernel >= ${MIN_MAJOR}.${MIN_MINOR}" "$rel: build one with --build-kernel"
    fi
    if grep -qi microsoft /proc/version 2>/dev/null; then
        row INFO "WSL" "$(sed 's/ (.*//' /proc/version)"
    fi
}

probe_io_uring() {
    local knob=/proc/sys/kernel/io_uring_disabled
    if [ -r "$knob" ]; then
        case "$(cat "$knob")" in
            0) row PASS "io_uring" "enabled (kernel.io_uring_disabled = 0)" ;;
            1) row WARN "io_uring" "root or the io_uring group only (kernel.io_uring_disabled = 1); dbladed runs as root" ;;
            *) row FAIL "io_uring" "disabled (kernel.io_uring_disabled = $(cat "$knob")); sysctl -w kernel.io_uring_disabled=0" ;;
        esac
    elif grep -q ' io_uring_setup$' /proc/kallsyms 2>/dev/null || grep -qw io_uring_setup /proc/kallsyms 2>/dev/null; then
        row PASS "io_uring" "present (no io_uring_disabled knob)"
    else
        row FAIL "io_uring" "no io_uring in this kernel"
    fi
}

probe_hugetlb() {
    if ! grep -qw hugetlbfs /proc/filesystems; then
        row FAIL "hugetlbfs" "not supported by this kernel (CONFIG_HUGETLBFS)"
        return
    fi
    local size
    size="$(awk '/^Hugepagesize:/ {print $2}' /proc/meminfo)"
    if [ "$size" = "2048" ]; then
        row PASS "hugetlbfs 2 MiB pages" "Hugepagesize ${size} kB"
    else
        row FAIL "hugetlbfs 2 MiB pages" "Hugepagesize ${size:-?} kB, the arena needs 2048"
    fi
    if grep -qw hugetlbfs /proc/mounts; then
        row PASS "hugetlbfs mounted" "$(awk '$3 == "hugetlbfs" {print $2}' /proc/mounts | head -n1)"
    else
        row WARN "hugetlbfs mounted" "none mounted; linux-smoke.sh mounts its own"
    fi
    local knob=/proc/sys/vm/nr_hugepages now
    now="$(cat "$knob")"
    if is_root; then
        if echo "$now" > "$knob" 2>/dev/null; then
            row PASS "nr_hugepages settable" "$now reserved now (cargo test needs about 32)"
        else
            row FAIL "nr_hugepages settable" "writing $knob was refused"
        fi
    else
        row WARN "nr_hugepages settable" "$now reserved; run the probe with sudo to test a write"
    fi
}

probe_ublk() {
    if is_root; then
        modprobe ublk_drv 2>/dev/null || true
    fi
    if [ -c /dev/ublk-control ]; then
        row PASS "/dev/ublk-control" "the ublk driver is loaded"
    elif modinfo ublk_drv >/dev/null 2>&1; then
        row WARN "/dev/ublk-control" "ublk_drv exists but is not loaded: sudo modprobe ublk_drv"
    else
        row FAIL "/dev/ublk-control" "no ublk_drv for $(uname -r): --build-kernel (CONFIG_BLK_DEV_UBLK=m), then install its modules"
    fi
    for m in dm-flakey dm-log-writes dm-delay; do
        if modinfo "$m" >/dev/null 2>&1 || [ -d "/sys/module/${m//-/_}" ]; then
            row PASS "module $m" "available (the crash lanes)"
        else
            row WARN "module $m" "missing (the crash lanes need it; --build-kernel adds dm-log-writes)"
        fi
    done
}

probe_tools() {
    local t
    for t in losetup blkdiscard cmp; do
        if command -v "$t" >/dev/null 2>&1; then
            row PASS "$t" "$(command -v "$t")"
        else
            row FAIL "$t" "missing: sudo scripts/wsl2-prep.sh --install-deps"
        fi
    done
    for t in fio jq; do
        if command -v "$t" >/dev/null 2>&1; then
            row PASS "$t" "$(command -v "$t")"
        else
            row WARN "$t" "missing (optional for the smoke test)"
        fi
    done
    local user home cargo
    user="$(build_user)"
    home="$(getent passwd "$user" | cut -d: -f6)"
    cargo="$(command -v cargo 2>/dev/null || true)"
    if [ -z "$cargo" ] && [ -x "$home/.cargo/bin/cargo" ]; then
        cargo="$home/.cargo/bin/cargo"
    fi
    if [ -n "$cargo" ]; then
        row PASS "cargo" "$("$cargo" --version 2>/dev/null || echo "$cargo") (for $user)"
    else
        row FAIL "cargo" "no Rust toolchain for $user: --install-deps, then rustup default stable"
    fi
}

probe_repo() {
    if [ -f "$WIN_REPO/Cargo.toml" ]; then
        row PASS "repository" "$WIN_REPO (build into an ext4 CARGO_TARGET_DIR, not under /mnt)"
    else
        row FAIL "repository" "$WIN_REPO/Cargo.toml not visible: is E: mounted at /mnt/e?"
    fi
    if [ "$(cat /proc/1/comm 2>/dev/null)" = "systemd" ]; then
        row INFO "init" "systemd"
    else
        row INFO "init" "$(cat /proc/1/comm 2>/dev/null || echo unknown) (systemd=true in /etc/wsl.conf enables systemd)"
    fi
}

do_probe() {
    probe_kernel
    probe_io_uring
    probe_hugetlb
    probe_ublk
    probe_tools
    probe_repo
    printf '%-5s  %-26s %s\n' "RESULT" "CHECK" "DETAIL"
    printf '%s\n' "${ROWS[@]}"
    echo
    if [ "$FAILS" -eq 0 ]; then
        echo "probe: everything required is present ($WARNS warning(s))"
        return 0
    fi
    echo "probe: $FAILS check(s) failed, $WARNS warning(s)"
    return 1
}

# --- the packages ------------------------------------------------------------

DEPS=(build-essential flex bison libssl-dev libelf-dev bc dwarves cpio python3
      util-linux fio jq kmod diffutils curl ca-certificates xz-utils git)

do_deps() {
    is_root || { echo "--install-deps needs root (sudo)" >&2; exit 2; }
    local codename
    codename="$(. /etc/os-release && echo "${VERSION_CODENAME:-unknown}")"
    if [ "$codename" = "bookworm" ]; then
        if [ "$DIST_UPGRADE" -eq 1 ]; then
            echo "upgrading bookworm to trixie (Debian 13)"
            if [ -f /etc/apt/sources.list ]; then
                sed -i 's/bookworm/trixie/g' /etc/apt/sources.list
            fi
            find /etc/apt/sources.list.d -type f \( -name '*.list' -o -name '*.sources' \) \
                -exec sed -i 's/bookworm/trixie/g' {} +
            apt-get update
            DEBIAN_FRONTEND=noninteractive apt-get -y full-upgrade
            codename="trixie"
        else
            echo "this image is Debian 12 (bookworm); OpenTAP's lane is Debian 13 (trixie)."
            echo "run again with --dist-upgrade to switch the sources to trixie and full-upgrade"
            echo "(the build itself works on bookworm too, except that it has no rustup package)."
        fi
    fi
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y "${DEPS[@]}"
    if apt-cache show rustup >/dev/null 2>&1; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y rustup
        local user
        user="$(build_user)"
        if [ "$user" != "root" ]; then
            if ! runuser -u "$user" -- bash -lc 'rustup toolchain list 2>/dev/null | grep -q stable'; then
                runuser -u "$user" -- bash -lc 'rustup default stable'
            fi
            echo "rustup: $(runuser -u "$user" -- bash -lc 'rustc --version' 2>/dev/null || echo 'stable installed')"
        else
            echo "rustup installed; as your own user run: rustup default stable"
        fi
    else
        echo "no rustup package on $codename: install Rust for your user from https://rustup.rs"
        echo "  (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y)"
    fi
    echo
    echo "packages installed; next: scripts/wsl2-prep.sh --build-kernel (as your user)"
}

# --- the kernel --------------------------------------------------------------

do_kernel() {
    if is_root && [ -n "${SUDO_USER:-}" ]; then
        echo "build the kernel as your own user, not under sudo" >&2
        exit 2
    fi
    WORK="${WORK:-$HOME/wsl-kernel}"
    case "$(realpath -m "$WORK")" in
        /mnt/*) echo "--work $WORK is on a Windows drive; build on the distro's ext4 disk" >&2; exit 2 ;;
    esac
    local t
    for t in make gcc flex bison bc tar curl pahole; do
        command -v "$t" >/dev/null 2>&1 || { echo "$t is missing: sudo scripts/wsl2-prep.sh --install-deps" >&2; exit 2; }
    done
    mkdir -p "$WORK"
    local tarball="$WORK/$TAG.tar.gz"
    local src="$WORK/src-$TAG"
    local staging="$WORK/modules-$TAG"
    if [ -f "$tarball" ] && gzip -t "$tarball" 2>/dev/null; then
        echo "[1/6] the tarball is here: $tarball"
    else
        echo "[1/6] fetching $REPO_URL tag $TAG"
        curl -fL --retry 3 -o "$tarball.part" "$REPO_URL/archive/refs/tags/$TAG.tar.gz"
        gzip -t "$tarball.part"
        mv "$tarball.part" "$tarball"
    fi
    if [ -f "$src/Makefile" ] && [ -f "$src/arch/x86/configs/config-wsl" ]; then
        echo "[2/6] the tree is unpacked: $src"
    else
        echo "[2/6] unpacking into $src"
        rm -rf "$src.part"
        mkdir -p "$src.part"
        tar -xzf "$tarball" -C "$src.part" --strip-components=1
        rm -rf "$src"
        mv "$src.part" "$src"
    fi
    echo "[3/6] config-wsl with CONFIG_BLK_DEV_UBLK=m and CONFIG_DM_LOG_WRITES=m"
    (
        cd "$src"
        cp arch/x86/configs/config-wsl .config
        scripts/config --module BLK_DEV_UBLK \
                       --module DM_LOG_WRITES \
                       --module DM_FLAKEY \
                       --module DM_DELAY \
                       --enable IO_URING \
                       --enable HUGETLBFS \
                       --set-str LOCALVERSION "-opentap"
        make olddefconfig >/dev/null
        for want in CONFIG_BLK_DEV_UBLK=m CONFIG_DM_LOG_WRITES=m CONFIG_IO_URING=y CONFIG_HUGETLBFS=y; do
            grep -qx "$want" .config || { echo "the config lost $want" >&2; exit 1; }
        done
    )
    echo "[4/6] building bzImage and modules with -j $JOBS (incremental when run again)"
    make -C "$src" -j "$JOBS" bzImage modules
    local release
    release="$(make -C "$src" -s --no-print-directory kernelrelease)"
    echo "[5/6] installing the modules of $release into $staging"
    rm -rf "$staging"
    make -C "$src" -s INSTALL_MOD_PATH="$staging" modules_install
    echo "[6/6] copying bzImage to $KERNEL_OUT"
    mkdir -p "$KERNEL_OUT"
    if [ -f "$KERNEL_OUT/bzImage" ] && ! cmp -s "$KERNEL_OUT/bzImage" "$src/arch/x86/boot/bzImage"; then
        cp "$KERNEL_OUT/bzImage" "$KERNEL_OUT/bzImage.prev"
    fi
    cp "$src/arch/x86/boot/bzImage" "$KERNEL_OUT/bzImage"
    local winpath
    winpath="$(echo "$KERNEL_OUT" | sed -E 's#^/mnt/([a-z])/#\U\1:\\\\#; s#/#\\\\#g')"
    echo
    echo "kernel $release is at $KERNEL_OUT/bzImage. On the Windows side:"
    echo "  1. in %USERPROFILE%\\.wslconfig, under [wsl2], set (replacing the commented line):"
    echo "         kernel=${winpath}\\\\bzImage"
    echo "  2. in PowerShell:  wsl --shutdown"
    echo "  3. open the distribution again; uname -r must say $release"
    echo "Then, in the distribution:"
    echo "  sudo cp -a $staging/lib/modules/$release /lib/modules/ && sudo depmod -a $release"
    echo "  sudo modprobe ublk_drv && ls -l /dev/ublk-control"
    echo "  scripts/wsl2-prep.sh --probe"
}

case "$MODE" in
    probe) do_probe ;;
    deps) do_deps ;;
    kernel) do_kernel ;;
esac
