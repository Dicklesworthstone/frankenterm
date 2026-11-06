#!/bin/bash
# ft-yccm0.2.1.7 AC4 native macOS: the mux-server-impl lib suite, then 1000 random crash
# schedules, on gate d7's tree (exported with git archive), as a debug test build without
# debuginfo. Prints a header, the test lines and the results.
cd ~/ft-build/budget-4wrv7/gate-src || exit 1
export RCH_CARGO_WRAPPER_BYPASS=1 RCH_REQUIRE_REMOTE=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export CARGO_TARGET_DIR=$HOME/ft-build/budget-4wrv7/target-gate-dbg
stamp() { echo "# $1 $(date -u +%Y-%m-%dT%H:%M:%SZ) load $(uptime | sed "s/.*averages: //")"; }
echo "# ft-yccm0.2.1.7 AC4 native macOS: mux-server-impl lib suite, then 1000 random crash schedules"
echo "# host: $(hostname -s) $(sw_vers -productVersion) $(uname -m); toolchain: $(rustc --version)"
echo "# source: gate 033c75af8 (tree 361e41bc4) = 6742c1532 plus the landed impl lib.rs and core mmap_store.rs"
echo "# filesystem: $(df -h ~/ft-build | tail -1 | cut -c1-40) $(diskutil info / 2>/dev/null | grep -m1 "Type (Bundle)" | sed "s/  */ /g")"
echo "# TMPDIR: ${TMPDIR:-unset}"
stamp "lib suite started"
cargo test -j 2 -p frankenterm-mux-server-impl --lib 2>&1 |
    grep -E "^test [A-Za-z0-9_:]+ \.\.\. |^test result: .* 0 filtered out|panicked"
stamp "lib suite finished; schedules started"
FT_CRASH_SCHEDULE_HISTORIES=1000 cargo test -j 2 -p frankenterm-mux-server-impl --lib -- \
    random_crash_schedules --nocapture 2>&1 | grep -E "^test |^test result|crash schedules:|panicked"
stamp "schedules finished"
