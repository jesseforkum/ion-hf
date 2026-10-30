#!/usr/bin/env bash
# Traced IONOS sessions, the stream gear against the same link without it
# (ION_STREAM=0), same seeds and length. Usage, from the worktree:
#   ION_SECS=900 bash lab_runs/stream_gear/trace.sh NAME "WGN 0" "MPG -6" ...
set -e
name=$1; shift
out=$(pwd)/lab_runs/stream_gear/$name.txt
: > "$out"
for cell in "$@"; do
    set -- $cell
    for on in ${ION_ARMS:-1 0}; do
        ION_STREAM=$on ION_MODE=$1 ION_SNR=$2 cargo test -p ion --release -j 1 --test stream stream_trace \
            -- --ignored --nocapture 2>&1 | grep "^TRACE" | sed "s/^TRACE/stream=$on/" >> "$out"
    done
done
