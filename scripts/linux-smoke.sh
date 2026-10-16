#!/usr/bin/env bash
# OpenTAP smoke test (Steps 5 and 6) for Debian 13 (kernel 6.12 or later),
# on a host or in WSL 2 with a ublk-enabled kernel (scripts/wsl2-prep.sh).
#
# Formats a pool on a loop device (or the block device you name), starts
# dbladed with a hugetlbfs arena, creates a volume, drives its ublk block
# device with dd (and fio when installed), takes a snapshot, overwrites and
# discards, rolls back and verifies, deletes the snapshot, runs the cleaner,
# restarts the daemon with SIGTERM (a clean boot), kills it with SIGKILL
# while a writer is in flight (the restarted daemon re-attaches the SAME
# /dev/ublkbN with UBLK_F_USER_RECOVERY, the writer finishes without an
# error and nothing is lost), orphans a device on purpose and finds it with
# `device list` and `device reap --stale`, verifies the pool with
# `pool verify`, pushes two snapshots over 127.0.0.1 to a second dbladed
# (`replicate push`) and compares the released mirror with the source byte
# for byte, and tears everything down.
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
# smoke test), --repl-port PORT (the second daemon's replication port on
# 127.0.0.1, default 47422). Under sudo the build runs as the calling user
# with that user's rustup toolchain; a repository on a Windows drive (/mnt)
# builds into /var/tmp/opentap-target-<user> unless CARGO_TARGET_DIR is set.
set -euo pipefail

DEVICE=""
SIZE="2G"
ARENA="256M"
HEAP=0
KEEP=0
PROFILE="debug"
REPL_PORT=47422
CARGO_FLAGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --device) DEVICE="$2"; shift 2 ;;
        --size) SIZE="$2"; shift 2 ;;
        --arena) ARENA="$2"; shift 2 ;;
        --heap-arena) HEAP=1; shift ;;
        --keep) KEEP=1; shift ;;
        --release) PROFILE="release"; CARGO_FLAGS=(--release); shift ;;
        --repl-port) REPL_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
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
STATE="$WORK/state"
ARENA_DIR="$WORK/arena"
ARENA_FILE="$ARENA_DIR/dbladed"
LOG2="$WORK/dbladed2.log"
SOCK2="$WORK/dbladed2.sock"
STATE2="$WORK/state2"
LOOP=""
LOOP2=""
DBLADED=""
OTAPCTL=""
DBLADED_PID=""
DBLADED2_PID=""
DD_PID=""
STEP=0
FAILED=1

say() { printf '\n[%02d] %s\n' "$((++STEP))" "$*"; }
die() { echo "FAILED: $*" >&2; exit 1; }

stop_daemon() {
    # stop_daemon PID SOCKET
    local pid="$1" sock="$2"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        if [ -n "$OTAPCTL" ]; then
            "$OTAPCTL" --control "unix:$sock" daemon stop >/dev/null 2>&1 || kill -TERM "$pid"
        else
            kill -TERM "$pid"
        fi
        wait "$pid" 2>/dev/null
    fi
}

cleanup() {
    set +e
    if [ -n "$DD_PID" ]; then
        kill "$DD_PID" 2>/dev/null
        wait "$DD_PID" 2>/dev/null
    fi
    stop_daemon "$DBLADED2_PID" "$SOCK2"
    stop_daemon "$DBLADED_PID" "$SOCK"
    if mountpoint -q "$ARENA_DIR" 2>/dev/null; then
        umount "$ARENA_DIR"
    fi
    if [ -n "$LOOP2" ]; then
        losetup -d "$LOOP2" 2>/dev/null
    fi
    if [ -n "$LOOP" ]; then
        losetup -d "$LOOP" 2>/dev/null
    fi
    if [ "$FAILED" -ne 0 ]; then
        echo "--- dbladed log ($LOG) ---" >&2
        tail -n 40 "$LOG" 2>/dev/null >&2
        if [ -s "$LOG2" ]; then
            echo "--- second dbladed log ($LOG2) ---" >&2
            tail -n 40 "$LOG2" 2>/dev/null >&2
        fi
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
[ -c /dev/ublk-control ] || die "/dev/ublk-control is missing: the ublk_drv module did not load (kernel 6.12+ with CONFIG_BLK_DEV_UBLK is needed; in WSL 2, scripts/wsl2-prep.sh --build-kernel)"
command -v losetup >/dev/null || die "losetup is missing (mount package)"
command -v blkdiscard >/dev/null || die "blkdiscard is missing (util-linux)"
command -v cmp >/dev/null || die "cmp is missing (diffutils)"
HAVE_FIO=0
command -v fio >/dev/null && HAVE_FIO=1
HAVE_JQ=0
command -v jq >/dev/null && HAVE_JQ=1

say "building dbladed and otapctl ($PROFILE)"
BUILD_USER="${SUDO_USER:-root}"
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    case "$ROOT" in
        /mnt/*) export CARGO_TARGET_DIR="/var/tmp/opentap-target-$BUILD_USER" ;;
    esac
fi
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
if [ "$BUILD_USER" != "root" ]; then
    # sudo resets PATH, which loses the user's rustup toolchain: build as
    # the user, with the user's cargo.
    BUILD_HOME="$(getent passwd "$BUILD_USER" | cut -d: -f6)"
    BUILD_ENV=("PATH=$BUILD_HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" "HOME=$BUILD_HOME")
    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
        BUILD_ENV+=("CARGO_TARGET_DIR=$CARGO_TARGET_DIR")
    fi
    runuser -u "$BUILD_USER" -- env "${BUILD_ENV[@]}" \
        bash -c 'cd "$1" && shift && cargo build "$@"' build "$ROOT" \
        "${CARGO_FLAGS[@]}" -p otap-daemon -p otap-ctl || die "the build failed"
else
    (cd "$ROOT" && cargo build "${CARGO_FLAGS[@]}" -p otap-daemon -p otap-ctl) || die "the build failed"
fi
DBLADED="$TARGET/$PROFILE/dbladed"
OTAPCTL="$TARGET/$PROFILE/otapctl"
[ -x "$DBLADED" ] && [ -x "$OTAPCTL" ] || die "the binaries are missing under $TARGET/$PROFILE"
ctl() { "$OTAPCTL" --control "unix:$SOCK" "$@"; }
ctl2() { "$OTAPCTL" --control "unix:$SOCK2" "$@"; }

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

wait_ping() {
    # wait_ping CTL PID LOGFILE
    local ctlf="$1" pid="$2" logf="$3"
    for _ in $(seq 1 100); do
        if "$ctlf" daemon ping >/dev/null 2>&1; then
            return 0
        fi
        kill -0 "$pid" 2>/dev/null || die "dbladed exited at start; see $logf"
        sleep 0.2
    done
    die "dbladed did not answer within 20 s; see $logf"
}

start_daemon() {
    # start_daemon [EXTRA DBLADED ARGS...]
    "$DBLADED" --control "unix:$SOCK" --device "$DEVICE" "${ARENA_ARGS[@]}" --cp-interval 2 \
        --state-dir "$STATE" "$@" >>"$LOG" 2>&1 &
    DBLADED_PID=$!
    wait_ping ctl "$DBLADED_PID" "$LOG"
}

device_of() {
    # device_of CTL VOLUME: the /dev/ublkbN path of a volume, from `volume show --json`.
    local json
    json="$("$1" --json volume show --name "$2")"
    if [ "$HAVE_JQ" -eq 1 ]; then
        echo "$json" | jq -r '.device'
    else
        echo "$json" | sed -n 's/.*"device": *"\([^"]*\)".*/\1/p'
    fi
}

wait_device_on() {
    # wait_device_on CTL VOLUME
    local dev
    for _ in $(seq 1 100); do
        dev="$(device_of "$1" "$2")"
        if [ -n "$dev" ] && [ "$dev" != "null" ] && [ -b "$dev" ]; then
            echo "$dev"
            return 0
        fi
        sleep 0.1
    done
    die "volume $2 has no block device"
}

wait_device() { wait_device_on ctl "$1"; }

format_with() {
    # format_with SOCKET DEVICE LOG: the format goes through a daemon with no pool booted.
    local sock="$1" dev="$2" logf="$3" pid
    "$DBLADED" --control "unix:$sock" >>"$logf" 2>&1 &
    pid=$!
    for _ in $(seq 1 100); do
        "$OTAPCTL" --control "unix:$sock" daemon ping >/dev/null 2>&1 && break
        sleep 0.2
    done
    "$OTAPCTL" --control "unix:$sock" pool format --device "$dev" --force
    "$OTAPCTL" --control "unix:$sock" daemon stop
    wait "$pid"
}

say "format"
"$DBLADED" --version
format_with "$SOCK" "$DEVICE" "$LOG"

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

if [ "$HEAP" -eq 1 ]; then
    say "SIGKILL: a crash with a heap arena"
    dd if=/dev/urandom of="$WORK/pattern2" bs=1M count=16 status=none
    dd if="$WORK/pattern2" of="$BLK" bs=1M seek=64 oflag=direct conv=fsync status=none
    kill -KILL "$DBLADED_PID"
    wait "$DBLADED_PID" 2>/dev/null || true
    DBLADED_PID=""
    sleep 1
    start_daemon
    STATUS="$(ctl pool status)"
    echo "$STATUS" | grep "last boot:"
    echo "$STATUS" | grep -q "last boot:    dirty boot" || die "a heap-arena crash must be a dirty boot"
    ctl volume show --name smoke | grep -q "nvfail" || die "the volume should be held by nvfail"
    ctl volume nvfail-clear --name smoke
    ctl volume show --name smoke | grep -q "state:      online" || die "the clear did not bring the volume back"
    BLK="$(wait_device smoke)"
    echo "the re-attach under I/O and the orphaned device need a hugetlbfs arena: skipped"
else
    say "SIGKILL under I/O: the restarted daemon re-attaches the same device"
    dd if=/dev/urandom of="$WORK/pattern2" bs=1M count=128 status=none
    OLD_BLK="$BLK"
    dd if="$WORK/pattern2" of="$BLK" bs=256k seek=256 oflag=direct conv=fsync status=none &
    DD_PID=$!
    sleep 0.5
    kill -KILL "$DBLADED_PID"
    wait "$DBLADED_PID" 2>/dev/null || true
    DBLADED_PID=""
    start_daemon
    STATUS="$(ctl pool status)"
    echo "$STATUS" | grep "last boot:"
    echo "$STATUS" | grep -q "arena re-attached" || die "the hugetlbfs arena did not re-attach"
    ctl volume show --name smoke | grep -q "state:      online" || die "a re-attached arena must not trip nvfail"
    BLK="$(wait_device smoke)"
    [ "$BLK" = "$OLD_BLK" ] || die "the restarted daemon exposes $BLK, not the re-attached $OLD_BLK"
    wait "$DD_PID" || die "the writer saw an error across the crash (requests the dead daemon held must be redelivered: UBLK_F_USER_RECOVERY_REISSUE)"
    DD_PID=""
    grep -E "serving .* after the arena re-attach began" "$LOG" | tail -n 1 || true
    dd if="$BLK" of="$WORK/after-kill" bs=256k skip=256 count=512 iflag=direct status=none
    cmp "$WORK/pattern2" "$WORK/after-kill" || die "acknowledged writes were lost across the crash"
    echo "the same $BLK came back and every write in flight landed"

    say "an orphaned device: listed stale and reaped"
    ctl volume create --name orphan --size 64M
    ORPHAN_BLK="$(wait_device orphan)"
    ORPHAN_ID="${ORPHAN_BLK#/dev/ublkb}"
    SMOKE_BLK="$BLK"
    kill -KILL "$DBLADED_PID"
    wait "$DBLADED_PID" 2>/dev/null || true
    DBLADED_PID=""
    # A boot without ublk forgets the volume while its device lives on.
    start_daemon --no-ublk
    ctl volume offline --name orphan
    ctl volume delete --name orphan
    ctl daemon stop
    wait "$DBLADED_PID" || die "dbladed exited with an error on daemon stop"
    DBLADED_PID=""
    start_daemon
    BLK="$(wait_device smoke)"
    [ "$BLK" = "$SMOKE_BLK" ] || die "the volume smoke came back as $BLK, not $SMOKE_BLK"
    ctl device list | tee "$WORK/devices"
    STATE_OF="$(awk -v id="$ORPHAN_ID" '$1 == id {print $3}' "$WORK/devices")"
    [ "$STATE_OF" = "stale" ] || die "the orphaned device $ORPHAN_BLK is listed as '${STATE_OF:-absent}', not stale"
    ctl device reap --stale
    for _ in $(seq 1 50); do
        [ -e "$ORPHAN_BLK" ] || break
        sleep 0.1
    done
    [ ! -e "$ORPHAN_BLK" ] || die "$ORPHAN_BLK is still there after the reap"
    ctl device list
    echo "the orphaned device was reaped; smoke kept $BLK"
fi

say "pool verify on a healthy pool"
VERIFY_EXIT=0
ctl pool verify >"$WORK/verify" || VERIFY_EXIT=$?
cat "$WORK/verify"
[ "$VERIFY_EXIT" -eq 0 ] && grep -q "clean" "$WORK/verify" || die "the verification did not conclude clean (otapctl exit $VERIFY_EXIT)"

say "replication: a push over 127.0.0.1 to a second dbladed, a delta, a byte-for-byte compare"
truncate -s 1G "$WORK/pool2.img"
LOOP2="$(losetup --find --show "$WORK/pool2.img")"
format_with "$SOCK2" "$LOOP2" "$LOG2"
"$DBLADED" --control "unix:$SOCK2" --device "$LOOP2" --heap-arena --arena-bytes 128M \
    --cp-interval 2 --state-dir "$STATE2" --replication-listen "127.0.0.1:$REPL_PORT" \
    >>"$LOG2" 2>&1 &
DBLADED2_PID=$!
wait_ping ctl2 "$DBLADED2_PID" "$LOG2"
grep -q "replication on tcp:127.0.0.1:$REPL_PORT" "$LOG2" || die "the second dbladed does not listen for replication"
ctl snapshot create --volume smoke --name r1
ctl replicate push smoke r1 --to "127.0.0.1:$REPL_PORT/mirror" || die "the first push did not complete"
ctl2 replicate status
ctl2 replicate cursor mirror
ctl2 volume show --name mirror | grep -q "offline" || die "the mirror must stay offline"
dd if=/dev/urandom of="$WORK/pattern3" bs=1M count=16 status=none
dd if="$WORK/pattern3" of="$BLK" bs=1M seek=400 oflag=direct conv=fsync status=none
blkdiscard -o $((8 * 1024 * 1024)) -l $((4 * 1024 * 1024)) "$BLK"
ctl snapshot create --volume smoke --name r2
ctl replicate push smoke r2 --to "127.0.0.1:$REPL_PORT/mirror" || die "the incremental push did not complete"
ctl replicate status
ctl replicate cursor smoke | grep -q "r2" || die "the source's replication base did not move to r2"
ctl2 replicate release mirror
ctl2 volume online --name mirror
MIRROR_BLK="$(wait_device_on ctl2 mirror)"
cmp "$BLK" "$MIRROR_BLK" || die "the mirror $MIRROR_BLK differs from the source $BLK"
echo "the mirror $MIRROR_BLK equals the source $BLK byte for byte"
ctl2 daemon stop
wait "$DBLADED2_PID" || die "the second dbladed exited with an error"
DBLADED2_PID=""
ctl snapshot replication-base --volume smoke --release
ctl snapshot delete --volume smoke --name r1
ctl snapshot delete --volume smoke --name r2

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
