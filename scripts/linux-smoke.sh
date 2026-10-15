#!/usr/bin/env bash
# OpenTAP Step 5 smoke test for Debian 13 (kernel 6.12 or later).
#
# Formats a pool on a loop device (or the block device you name), starts
# dbladed with a hugetlbfs arena, creates a volume, drives its ublk block
# device with dd (and fio when installed), takes a snapshot, overwrites and
# discards, rolls back and verifies, deletes the snapshot, runs the cleaner,
# restarts the daemon with SIGTERM (a clean boot), kills it with SIGKILL
# (a dirty boot whose hugetlbfs arena re-attaches, so nothing is lost),
# verifies the data once more, and tears everything down.
#
# Run as root from the repository root:
#
#     sudo scripts/linux-smoke.sh                 # a 2 GiB loop device
#     sudo scripts/linux-smoke.sh --device /dev/sdX   # a real device: WIPED
#     sudo scripts/linux-smoke.sh --heap-arena    # no huge pages reserved
#
# Options: --device PATH, --size SIZE (loop file, default 2G), --arena SIZE
# (default 256M), --heap-arena, --keep (leave the work directory), --release
# (build with --release; the default is the dev profile, which is fine for a
# smoke test).
set -euo pipefail

DEVICE=""
SIZE="2G"
ARENA="256M"
HEAP=0
KEEP=0
PROFILE="debug"
CARGO_FLAGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --device) DEVICE="$2"; shift 2 ;;
        --size) SIZE="$2"; shift 2 ;;
        --arena) ARENA="$2"; shift 2 ;;
        --heap-arena) HEAP=1; shift ;;
        --keep) KEEP=1; shift ;;
        --release) PROFILE="release"; CARGO_FLAGS=(--release); shift ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "run as root (ublk and loop devices need it)" >&2
    exit 2
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d /var/tmp/opentap-smoke.XXXXXX)"
LOG="$WORK/dbladed.log"
SOCK="$WORK/dbladed.sock"
ARENA_DIR="$WORK/arena"
ARENA_FILE="$ARENA_DIR/dbladed"
LOOP=""
DBLADED=""
OTAPCTL=""
DBLADED_PID=""
STEP=0
FAILED=1

say() { printf '\n[%02d] %s\n' "$((++STEP))" "$*"; }
die() { echo "FAILED: $*" >&2; exit 1; }

cleanup() {
    set +e
    if [ -n "$DBLADED_PID" ] && kill -0 "$DBLADED_PID" 2>/dev/null; then
        if [ -n "$OTAPCTL" ]; then
            "$OTAPCTL" --control "unix:$SOCK" daemon stop >/dev/null 2>&1 || kill -TERM "$DBLADED_PID"
        else
            kill -TERM "$DBLADED_PID"
        fi
        wait "$DBLADED_PID" 2>/dev/null
    fi
    if mountpoint -q "$ARENA_DIR" 2>/dev/null; then
        umount "$ARENA_DIR"
    fi
    if [ -n "$LOOP" ]; then
        losetup -d "$LOOP" 2>/dev/null
    fi
    if [ "$FAILED" -ne 0 ]; then
        echo "--- dbladed log ($LOG) ---" >&2
        tail -n 40 "$LOG" 2>/dev/null >&2
        echo "work directory kept at $WORK" >&2
    elif [ "$KEEP" -eq 0 ]; then
        rm -rf "$WORK"
    else
        echo "work directory kept at $WORK"
    fi
}
trap cleanup EXIT

say "preconditions"
KERNEL="$(uname -r)"
echo "kernel $KERNEL"
modprobe ublk_drv 2>/dev/null || true
[ -c /dev/ublk-control ] || die "/dev/ublk-control is missing: the ublk_drv module did not load (kernel 6.12+ with CONFIG_BLK_DEV_UBLK is needed)"
command -v losetup >/dev/null || die "losetup is missing (mount package)"
command -v blkdiscard >/dev/null || die "blkdiscard is missing (util-linux)"
command -v cmp >/dev/null || die "cmp is missing (diffutils)"
HAVE_FIO=0
command -v fio >/dev/null && HAVE_FIO=1
HAVE_JQ=0
command -v jq >/dev/null && HAVE_JQ=1

say "building dbladed and otapctl ($PROFILE)"
(cd "$ROOT" && cargo build "${CARGO_FLAGS[@]}" -p otap-daemon -p otap-ctl) || die "the build failed"
DBLADED="$ROOT/target/$PROFILE/dbladed"
OTAPCTL="$ROOT/target/$PROFILE/otapctl"
[ -x "$DBLADED" ] && [ -x "$OTAPCTL" ] || die "the binaries are missing under target/$PROFILE"
ctl() { "$OTAPCTL" --control "unix:$SOCK" "$@"; }

say "the device"
if [ -z "$DEVICE" ]; then
    truncate -s "$SIZE" "$WORK/pool.img"
    LOOP="$(losetup --find --show --direct-io=on "$WORK/pool.img" 2>/dev/null || losetup --find --show "$WORK/pool.img")"
    DEVICE="$LOOP"
    echo "loop device $DEVICE over $WORK/pool.img ($SIZE)"
else
    [ -b "$DEVICE" ] || die "$DEVICE is not a block device"
    echo "WIPING $DEVICE in 5 seconds (Ctrl-C to abort)"
    sleep 5
fi

ARENA_ARGS=(--arena-bytes "$ARENA")
if [ "$HEAP" -eq 1 ]; then
    ARENA_ARGS+=(--heap-arena)
    echo "heap arena ($ARENA): a crash loses the unflushed writes and the next boot holds the volumes under nvfail"
else
    say "huge pages for a $ARENA arena"
    PAGES=$(( ($(numfmt --from=iec "$ARENA") + 2097151) / 2097152 + 8 ))
    HAVE=$(cat /proc/sys/vm/nr_hugepages)
    if [ "$HAVE" -lt "$PAGES" ]; then
        echo "$PAGES" > /proc/sys/vm/nr_hugepages
    fi
    GOT=$(cat /proc/sys/vm/nr_hugepages)
    [ "$GOT" -ge "$PAGES" ] || die "only $GOT of $PAGES 2 MiB huge pages could be reserved; free memory or use --heap-arena"
    mkdir -p "$ARENA_DIR"
    mount -t hugetlbfs -o pagesize=2M,size="$ARENA" none "$ARENA_DIR" || die "cannot mount hugetlbfs"
    ARENA_ARGS+=(--arena-file "$ARENA_FILE")
    echo "$GOT huge pages, hugetlbfs at $ARENA_DIR"
fi

start_daemon() {
    "$DBLADED" --control "unix:$SOCK" --device "$DEVICE" "${ARENA_ARGS[@]}" --cp-interval 2 >>"$LOG" 2>&1 &
    DBLADED_PID=$!
    for _ in $(seq 1 100); do
        if ctl daemon ping >/dev/null 2>&1; then
            return 0
        fi
        kill -0 "$DBLADED_PID" 2>/dev/null || die "dbladed exited at start; see $LOG"
        sleep 0.2
    done
    die "dbladed did not answer on $SOCK within 20 s"
}

volume_device() {
    # The /dev/ublkbN path of a volume, from `volume show --json`.
    local json
    json="$(ctl --json volume show --name "$1")"
    if [ "$HAVE_JQ" -eq 1 ]; then
        echo "$json" | jq -r '.device'
    else
        echo "$json" | sed -n 's/.*"device": *"\([^"]*\)".*/\1/p'
    fi
}

wait_device() {
    local dev
    for _ in $(seq 1 100); do
        dev="$(volume_device "$1")"
        if [ -n "$dev" ] && [ "$dev" != "null" ] && [ -b "$dev" ]; then
            echo "$dev"
            return 0
        fi
        sleep 0.1
    done
    die "volume $1 has no block device"
}

say "format"
"$DBLADED" --version
ctl_format() {
    # The format goes through a daemon with no pool booted.
    "$DBLADED" --control "unix:$SOCK" >>"$LOG" 2>&1 &
    DBLADED_PID=$!
    for _ in $(seq 1 100); do ctl daemon ping >/dev/null 2>&1 && break; sleep 0.2; done
    ctl pool format --device "$DEVICE" --force
    ctl daemon stop
    wait "$DBLADED_PID"
    DBLADED_PID=""
}
ctl_format

say "boot"
start_daemon
ctl pool status

say "a volume and its block device"
ctl volume create --name smoke --size 512M
BLK="$(wait_device smoke)"
echo "volume smoke is $BLK"
ctl volume list
blockdev --getsize64 "$BLK" | grep -qx 536870912 || die "$BLK is not 512 MiB"

say "write 64 MiB, read it back"
dd if=/dev/urandom of="$WORK/pattern" bs=1M count=64 status=none
dd if="$WORK/pattern" of="$BLK" bs=1M oflag=direct conv=fsync status=none
dd if="$BLK" of="$WORK/readback" bs=1M count=64 iflag=direct status=none
cmp "$WORK/pattern" "$WORK/readback" || die "the readback differs"
echo "64 MiB verified"
if [ "$HAVE_FIO" -eq 1 ]; then
    say "fio: 256 MiB of 4 KiB random writes, then verification"
    fio --name=smoke --filename="$BLK" --rw=randwrite --bs=4k --iodepth=16 --ioengine=libaio \
        --direct=1 --offset=128M --size=256M --verify=crc32c --do_verify=1 --verify_fatal=1 \
        --minimal >/dev/null || die "fio verification failed"
    echo "fio verified"
    dd if="$WORK/pattern" of="$BLK" bs=1M oflag=direct conv=fsync status=none
fi

say "snapshot, overwrite, discard, roll back, verify"
ctl snapshot create --volume smoke --name s1
dd if=/dev/zero of="$BLK" bs=1M count=8 oflag=direct conv=fsync status=none
blkdiscard -o $((16 * 1024 * 1024)) -l $((8 * 1024 * 1024)) "$BLK"
dd if="$BLK" of="$WORK/changed" bs=1M count=64 iflag=direct status=none
cmp -s "$WORK/pattern" "$WORK/changed" && die "the overwrite did not land"
ctl snapshot list
ctl snapshot rollback --volume smoke --name s1
BLK="$(wait_device smoke)"
dd if="$BLK" of="$WORK/rolled" bs=1M count=64 iflag=direct status=none
cmp "$WORK/pattern" "$WORK/rolled" || die "the rollback did not restore the pattern"
echo "rollback verified on $BLK"

say "delete the snapshot, run the cleaner"
ctl snapshot delete --volume smoke --name s1
ctl pool clean
ctl pool status

say "SIGTERM: a clean shutdown and restart"
kill -TERM "$DBLADED_PID"
wait "$DBLADED_PID" || die "dbladed exited with an error on SIGTERM"
DBLADED_PID=""
grep -q "stopped cleanly" "$LOG" || die "the log does not record a clean stop"
start_daemon
ctl pool status | grep -q "last boot:    clean boot" || die "the restart was not a clean boot"
BLK="$(wait_device smoke)"
dd if="$BLK" of="$WORK/after-term" bs=1M count=64 iflag=direct status=none
cmp "$WORK/pattern" "$WORK/after-term" || die "the data changed across the clean restart"
echo "clean restart verified"

say "SIGKILL: a crash"
dd if=/dev/urandom of="$WORK/pattern2" bs=1M count=16 status=none
dd if="$WORK/pattern2" of="$BLK" bs=1M seek=64 oflag=direct conv=fsync status=none
OLD_BLK="$BLK"
kill -KILL "$DBLADED_PID"
wait "$DBLADED_PID" 2>/dev/null || true
DBLADED_PID=""
sleep 1
start_daemon
echo "note: the killed daemon's device $OLD_BLK stays quiesced under UBLK_F_USER_RECOVERY until a"
echo "      tool deletes it or the module is reloaded; the restarted daemon exposes new devices"
echo "      (the recovery re-attach is a Step 6 item)"
STATUS="$(ctl pool status)"
echo "$STATUS" | grep "last boot:"
if [ "$HEAP" -eq 1 ]; then
    echo "$STATUS" | grep -q "last boot:    dirty boot" || die "a heap-arena crash must be a dirty boot"
    ctl volume show --name smoke | grep -q "nvfail" || die "the volume should be held by nvfail"
    ctl volume nvfail-clear --name smoke
    ctl volume show --name smoke | grep -q "state:      online" || die "the clear did not bring the volume back"
else
    echo "$STATUS" | grep -q "arena re-attached" || die "the hugetlbfs arena did not re-attach"
    ctl volume show --name smoke | grep -q "state:      online" || die "a re-attached arena must not trip nvfail"
    BLK="$(wait_device smoke)"
    dd if="$BLK" of="$WORK/after-kill" bs=1M skip=64 count=16 iflag=direct status=none
    cmp "$WORK/pattern2" "$WORK/after-kill" || die "acknowledged writes were lost across the crash"
    echo "the acknowledged writes survived the crash"
fi

say "teardown"
ctl volume offline --name smoke
ctl volume delete --name smoke
ctl volume list
ctl daemon stop
wait "$DBLADED_PID" || die "dbladed exited with an error on daemon stop"
DBLADED_PID=""
FAILED=0
echo
echo "SMOKE TEST PASSED on $KERNEL ($DEVICE, arena $ARENA$( [ "$HEAP" -eq 1 ] && echo ' heap' || echo ' hugetlbfs'))"
