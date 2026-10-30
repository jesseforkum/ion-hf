#!/usr/bin/env bash
# IONOS sessions, link (main) against Data2G's whole-burst clipper (PR #1).
# Usage, from the repo: bash lab_runs/tdclip/run.sh NAME [VAR=VALUE ...]
set -e
name=$1
shift
out=$(pwd)/lab_runs/tdclip/$name.txt
export ION_IONOS_MODES=${ION_IONOS_MODES:-WGN,MPG,MPP,MPD}
export ION_SNRS=${ION_SNRS:--6,-3,0,3,6,10,20}
export ION_MINUTES=${ION_MINUTES:-5}
export ION_SEEDS=${ION_SEEDS:-2}
export ION_THREADS=${ION_THREADS:-8}
export ION_AIR_MAX=${ION_AIR_MAX:--100}
export CARGO_TARGET_DIR=$(pwd)/target
env "$@" cargo test -p ion --release --test ionos -- --ignored --nocapture >"$out" 2>&1
