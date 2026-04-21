//! Corpus-replay integration tests.
//!
//! Reads every seed file from fuzz/corpus/ and feeds it through the
//! corresponding parser. This catches panics that the fuzzer would find
//! without needing a sanitizer build.

use std::fs;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fuzz")
        .join("corpus")
}

fn read_seeds(target: &str) -> Vec<(String, Vec<u8>)> {
    let dir = corpus_dir().join(target);
    if !dir.exists() {
        return Vec::new();
    }
    let mut seeds: Vec<(String, Vec<u8>)> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let e = e.ok()?;
            if e.file_type().ok()?.is_file() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    return None;
                }
                let data = fs::read(e.path()).ok()?;
                Some((name, data))
            } else {
                None
            }
        })
        .collect();
    seeds.sort_by(|a, b| a.0.cmp(&b.0));
    seeds
}

// ── scan_pipeline_quick ──────────────────────────────────────────────────

#[test]
fn replay_scan_pipeline_quick() {
    use frankenterm_core::scan_pipeline::quick_scan;

    let seeds = read_seeds("scan_pipeline_quick");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    for (name, data) in &seeds {
        if data.len() > 128_000 {
            continue;
        }
        let output = quick_scan(data);
        assert_eq!(
            output.input_bytes,
            data.len() as u64,
            "seed {name}: input_bytes mismatch"
        );
    }
}

// ── config_toml_parser ───────────────────────────────────────────────────

#[test]
fn replay_config_toml_parser() {
    use frankenterm_core::config::Config;

    let seeds = read_seeds("config_toml_parser");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    for (name, data) in &seeds {
        if data.len() > 32_768 {
            continue;
        }
        let text = match std::str::from_utf8(&data) {
            Ok(t) => t,
            Err(_) => continue,
        };
        // Must not panic — errors are fine.
        let _ = Config::from_toml(text);
        let _ = format!("seed {name} OK");
    }
}

// ── tuning_config_toml ──────────────────────────────────────────────────

#[test]
fn replay_tuning_config_toml() {
    use frankenterm_core::tuning_config::TuningConfig;

    let seeds = read_seeds("tuning_config_toml");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    for (name, data) in &seeds {
        if data.len() > 16_384 {
            continue;
        }
        let text = match std::str::from_utf8(&data) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let _: Result<TuningConfig, _> = toml::from_str(text);
        let _ = format!("seed {name} OK");
    }
}

// ── osc_marker_parser ───────────────────────────────────────────────────

#[test]
fn replay_osc_marker_parser() {
    use frankenterm_core::ingest::{Osc133State, parse_osc133_markers};

    let seeds = read_seeds("osc_marker_parser");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    for (name, data) in &seeds {
        if data.len() > 64_000 {
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        let markers = parse_osc133_markers(&text);
        let mut state = Osc133State::new();
        for marker in markers {
            state.process_marker(marker);
        }
        let _ = state.state.is_at_prompt();
        let _ = format!("seed {name} OK");
    }
}

// ── string_parsers ──────────────────────────────────────────────────────

#[test]
fn replay_string_parsers() {
    use frankenterm_core::caut::CautService;
    use frankenterm_core::event_stream::SeverityLevel;

    let seeds = read_seeds("string_parsers");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    for (name, data) in &seeds {
        if data.len() > 4_096 {
            continue;
        }
        let text = match std::str::from_utf8(&data) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let _ = CautService::from_cli_input(text);
        let _ = SeverityLevel::from_str_loose(text);
        let _ = format!("seed {name} OK");
    }
}

// ── recorder_event_json ─────────────────────────────────────────────────

#[test]
fn replay_recorder_event_json() {
    use frankenterm_core::recording::parse_recorder_event_json;

    let seeds = read_seeds("recorder_event_json");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    for (name, data) in &seeds {
        if data.len() > 16_384 {
            continue;
        }
        let text = match std::str::from_utf8(&data) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let _ = parse_recorder_event_json(text);
        let _ = format!("seed {name} OK");
    }
}

// ── ntm_importer_json ───────────────────────────────────────────────────

#[test]
fn replay_ntm_importer_json() {
    use frankenterm_core::ntm_importer::{
        parse_ntm_config, parse_ntm_sessions, parse_ntm_workflows,
    };

    let seeds = read_seeds("ntm_importer_json");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    for (name, data) in &seeds {
        if data.len() > 65_536 {
            continue;
        }
        let text = match std::str::from_utf8(&data) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let _ = parse_ntm_sessions(text);
        let _ = parse_ntm_workflows(text);
        let _ = parse_ntm_config(text);
        let _ = format!("seed {name} OK");
    }
}

// ── pattern_pack_parser ─────────────────────────────────────────────────

#[test]
fn replay_pattern_pack_parser() {
    use frankenterm_core::patterns::{PatternEngine, PatternPack, RuleDef};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct PackToml {
        name: String,
        version: String,
        rules: Vec<RuleDef>,
    }

    let seeds = read_seeds("pattern_pack_parser");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    for (name, data) in &seeds {
        if data.len() > 16_384 {
            continue;
        }
        let text = match std::str::from_utf8(&data) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let pack: PackToml = match toml::from_str(text) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let pack = PatternPack::new(pack.name, pack.version, pack.rules);
        let _ = PatternEngine::with_packs(vec![pack]);
        let _ = format!("seed {name} OK");
    }
}

// ── fts_query ────────────────────────────────────────────────────────────

#[test]
fn replay_fts_query() {
    use frankenterm_core::storage::initialize_schema;
    use rusqlite::Connection;

    let seeds = read_seeds("fts_query");
    assert!(!seeds.is_empty(), "no corpus seeds found");

    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO panes (pane_id, first_seen_at, last_seen_at) VALUES (1, 0, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (1, 0, 'seed', 4, 0)",
        [],
    )
    .unwrap();

    for (name, data) in &seeds {
        if data.len() > 8_192 {
            continue;
        }
        let query = match std::str::from_utf8(&data) {
            Ok(t) => t,
            Err(_) => continue,
        };
        // FTS5 MATCH queries — errors are fine, panics are not.
        let _ = conn.query_row(
            "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH ?1 LIMIT 1",
            rusqlite::params![query],
            |_| Ok::<_, rusqlite::Error>(()),
        );
        let _ = format!("seed {name} OK");
    }
}
