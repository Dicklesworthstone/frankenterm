#!/bin/bash
set -e

# Get mapping from title to ID
br list --json | jq -r '.[] | "\(.id)|\(.title)"' > issues.tmp

function get_id() {
    local title="$1"
    grep "$title" issues.tmp | head -n 1 | cut -d'|' -f1 || true
}

# Epic 1
EPIC1=$(get_id "Epic: Eradicate Panic Surfaces in Library Code")
T1_1=$(get_id "Remove panic! macros from replay_capture.rs")
T1_2=$(get_id "Replace Mutex::lock().unwrap() poison handling")
T1_3=$(get_id "Audit and remove serde_json::from_str(...).unwrap()")
if [ -n "$EPIC1" ]; then
    [ -n "$T1_1" ] && br dep add $T1_1 $EPIC1
    [ -n "$T1_2" ] && br dep add $T1_2 $EPIC1
    [ -n "$T1_3" ] && br dep add $T1_3 $EPIC1
fi

# Epic 2
EPIC2=$(get_id "Epic: Concurrency and Async Safety")
T2_1=$(get_id "Eliminate async lock guards held across await points")
T2_2=$(get_id "Fix unjoined std::thread::spawn calls")
T2_3=$(get_id "Enforce TcpStream shutdown")
if [ -n "$EPIC2" ]; then
    [ -n "$T2_1" ] && br dep add $T2_1 $EPIC2
    [ -n "$T2_2" ] && br dep add $T2_2 $EPIC2
    [ -n "$T2_3" ] && br dep add $T2_3 $EPIC2
fi

# Epic 3
EPIC3=$(get_id "Epic: Memory Safety and Legacy Cleanup")
T3_1=$(get_id "Refactor mem::transmute in legacy teletypewriter and vtparse")
T3_2=$(get_id "Remove mem::forget from session_restore.rs")
if [ -n "$EPIC3" ]; then
    [ -n "$T3_1" ] && br dep add $T3_1 $EPIC3
    [ -n "$T3_2" ] && br dep add $T3_2 $EPIC3
fi

# Epic 4
EPIC4=$(get_id "Epic: Robustness and Performance Hotspots")
T4_1=$(get_id "Hoist Regex::new out of loops")
T4_2=$(get_id "Guard division and modulo operations against zero")
if [ -n "$EPIC4" ]; then
    [ -n "$T4_1" ] && br dep add $T4_1 $EPIC4
    [ -n "$T4_2" ] && br dep add $T4_2 $EPIC4
fi

br sync --flush-only
rm issues.tmp
