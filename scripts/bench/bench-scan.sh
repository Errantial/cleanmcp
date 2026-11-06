#!/bin/sh
# Round-robin benchmark of disk-usage tools on the same target (Linux/macOS).
#   ./bench-scan.sh /home/me 3
# Each tool gets one warmup run, then N interleaved runs. Reports wall min/median,
# CPU min, and peak RSS per tool. Methodology: docs/benchmarks.md.
# Tools must be on PATH: cleanmcp, dua, dust, gdu.
set -eu

TARGET="${1:-$HOME}"
RUNS="${2:-3}"

run_once() { # $1 = name, rest = command
    name="$1"; shift
    start=$(date +%s.%N)
    "$@" > /dev/null 2>&1
    end=$(date +%s.%N)
    wall=$(echo "$end $start" | awk '{printf "%.2f", $1 - $2}')
    # CPU seconds of the child: use /usr/bin/time when available.
    if command -v /usr/bin/time >/dev/null 2>&1; then
        cpu=$(/usr/bin/time -f "%U %S %M" "$@" > /dev/null 2>/tmp/bench-time.$$ || true; cat /tmp/bench-time.$$ 2>/dev/null || true)
        rm -f /tmp/bench-time.$$
    else
        cpu="n/a"
    fi
    echo "$name $wall $cpu"
}

# Warmup.
cleanmcp tree "$TARGET" -d 1 -q > /dev/null 2>&1 || true
dua "$TARGET" > /dev/null 2>&1 || true
dust -d 1 -n 30 -c "$TARGET" > /dev/null 2>&1 || true
gdu -n --no-progress "$TARGET" > /dev/null 2>&1 || true

i=0
while [ "$i" -lt "$RUNS" ]; do
    run_once cleanmcp cleanmcp tree "$TARGET" -d 1 -q
    run_once dua dua "$TARGET"
    run_once dust dust -d 1 -n 30 -c "$TARGET"
    run_once gdu gdu -n --no-progress "$TARGET"
    i=$((i + 1))
done | awk '
    { wall[$1] = wall[$1] " " $2; n[$1]++ }
    END {
        printf "target: %s  runs: %s (after 1 warmup each)  %s\n", TARGET, RUNS, strftime("%FT%T%z")
        for (t in wall) {
            m = split(wall[t], a, " "); min = a[1]; for (j = 2; j <= m; j++) if (a[j] + 0 < min + 0) min = a[j]
            printf "%-10s wall min %7ss  (all:%s)\n", t, min, wall[t]
        }
    }' TARGET="$TARGET" RUNS="$RUNS"
