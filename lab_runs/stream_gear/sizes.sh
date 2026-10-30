#!/usr/bin/env bash
# The mixed-size table (stream_sizes), one cell after another. Usage, from
# the worktree: bash lab_runs/stream_gear/sizes.sh NAME "WGN 0" "MPG -3" ...
set -e
name=$1; shift
out=$(pwd)/lab_runs/stream_gear/$name.txt
: > "$out"
for cell in "$@"; do
    set -- $cell
    ION_MODE=$1 ION_SNR=$2 cargo test -p ion --release -j 1 --test stream stream_sizes \
        -- --ignored --nocapture 2>&1 | grep "^SIZE" >> "$out"
done
