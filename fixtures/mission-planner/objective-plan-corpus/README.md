# Mission Objective Plan Golden Corpus

This directory contains deterministic mission objective planner inputs and a
reviewed manifest of expected contract fields. The harness in
`crates/frankenterm-core/tests/mission_objective_plan_golden_corpus.rs` reads
these fixtures, invokes the real side-effect-free planner, validates generated
plan JSON against `docs/json-schema/ft-mission-objective-plan.json`, checks TOON
determinism, and verifies retained artifact hashes.

The retained artifacts are redacted command-output fixtures or prior reviewed
contract examples. They record source commands, exit codes, scrub rules, and
hashes so drift is intentional rather than accidental. Retained negative
artifacts pin reviewed rejection examples, including raw pane content storage
that must keep failing the objective-plan schema.
