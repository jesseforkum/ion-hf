#!/usr/bin/env bash
# IONOS sessions, alpha.6 against alpha.5. Usage, from the repo:
#   bash lab_runs/alpha6/run.sh TREE NAME [VAR=VALUE ...]
# runs tests/ionos.rs in worktree TREE (its own target directory) and
# writes lab_runs/alpha6/NAME.txt here. ION_THREADS (default 4).
set -e
tree=$1
name=$2
shift 2
out=$(pwd)/lab_runs/alpha6/$name.txt
export ION_IONOS_MODES=${ION_IONOS_MODES:-WGN,MPG,MPP,MPD}
export ION_SNRS=${ION_SNRS:--6,-3,0,3,6,10,20}
export ION_MINUTES=${ION_MINUTES:-5}
export ION_SEEDS=${ION_SEEDS:-2}
export ION_THREADS=${ION_THREADS:-4}
export ION_AIR_MAX=${ION_AIR_MAX:--100}
cd "$tree"
export CARGO_TARGET_DIR=$(pwd)/target
env "$@" cargo test -p ion --release -j "$ION_THREADS" --test ionos -- --ignored --nocapture >"$out" 2>&1
