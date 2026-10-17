//! Headless conformance runner (ft-yccm0.1.9): recorded vttest sessions and
//! esctest-style cases against frankenterm-term, compared with golden
//! expectations under `fixtures/conformance`. See `tests/conformance/mod.rs`
//! for how to run it against a candidate engine.
//!
//! The self-tests plant a known-bad golden, a broken engine and bad xfail
//! entries, and require the runner to catch each one, so the suite cannot
//! pass vacuously.

#[path = "differential/mod.rs"]
#[allow(dead_code)]
mod differential;

#[path = "conformance/mod.rs"]
#[allow(dead_code)]
mod conformance;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use conformance::cases;
use conformance::dump::{self, ScreenDump};
use conformance::suite::{self, Golden, Outcome, Status};
use conformance::vtrec::{self, Recording};
use differential::engine::{
    drain_replies, new_terminal_with, Engine, EngineFactory, EngineIo, Geometry, Legacy,
};
use differential::snapshot::{self, EngineSnapshot};
use frankenterm_term::Terminal;

fn out_dir() -> PathBuf {
    std::env::var_os("FT_CONFORMANCE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")))
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "conformance-{}-{}",
        name,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

fn recordings() -> Vec<Recording> {
    vtrec::load_dir(&suite::recordings_dir(&suite::fixtures_dir()))
        .unwrap_or_else(|err| panic!("{}", err))
}

fn recording(session: &str) -> Recording {
    recordings()
        .into_iter()
        .find(|recording| recording.session == session)
        .unwrap_or_else(|| panic!("no {} recording", session))
}

fn every_case_id() -> Vec<String> {
    let mut ids = Vec::new();
    for recording in recordings() {
        for index in 0..recording.screens.len() {
            ids.push(suite::vttest_id(&recording.session, index));
        }
    }
    for case in cases::all() {
        ids.push(suite::esctest_id(&case));
    }
    ids
}

fn write_golden(dir: &Path, recording: &Recording, text: &str, attrs: &str) {
    std::fs::write(dir.join(format!("{}.screens.txt", recording.session)), text)
        .expect("write screens");
    std::fs::write(dir.join(format!("{}.attrs.json", recording.session)), attrs)
        .expect("write attrs");
}

fn keys(recording: &Recording) -> Vec<String> {
    recording
        .screens
        .iter()
        .map(|screen| screen.key.clone())
        .collect()
}

fn print_review_files(run: &suite::SuiteRun, verdicts: &[suite::Verdict]) {
    for (session, keys, dumps) in &run.sessions {
        println!("----- BEGIN vttest/goldens/{}.screens.txt -----", session);
        print!(
            "{}",
            dump::render_text_file(&format!("vttest/{}", session), keys, dumps)
        );
        println!("----- END -----");
        println!("----- BEGIN vttest/goldens/{}.attrs.json -----", session);
        print!("{}", dump::render_attrs_file(session, dumps));
        println!("----- END -----");
    }
    println!("----- BEGIN baseline/legacy.txt -----");
    print!("{}", suite::render_baseline(verdicts));
    println!("----- END -----");
}

#[test]
fn conformance_suite_matches_the_committed_baseline() {
    let started = Instant::now();
    let fixtures = suite::fixtures_dir();
    let factory = suite::selected_engine().unwrap_or_else(|err| panic!("{}", err));
    let grid = suite::grid_engine();
    let run = suite::run_suite(&*factory, &fixtures).unwrap_or_else(|err| panic!("{}", err));
    let xfails = suite::load_xfails(&fixtures).unwrap_or_else(|err| panic!("{}", err));
    let verdicts = suite::classify(&run.outcomes, &xfails).unwrap_or_else(|err| panic!("{}", err));

    let report = suite::render_report(factory.name(), &grid, &verdicts);
    let out = out_dir();
    std::fs::create_dir_all(&out).expect("create the report directory");
    let report_path = out.join(format!("conformance-{}-grid-{}.txt", factory.name(), grid));
    std::fs::write(&report_path, &report).expect("write the report");
    println!("{}", report);
    println!("report: {}", report_path.display());
    if std::env::var("FT_CONFORMANCE_PRINT").as_deref() == Ok("1") {
        print_review_files(&run, &verdicts);
    }

    let baseline_path = suite::baseline_path(&fixtures);
    let baseline_text = std::fs::read_to_string(&baseline_path)
        .unwrap_or_else(|err| panic!("{}: {}", baseline_path.display(), err));
    let baseline = suite::parse_baseline(&baseline_text).unwrap_or_else(|err| panic!("{}", err));
    let strict = factory.name() == "legacy" && grid == "legacy";
    let deltas = suite::compare_with_baseline(&baseline, &verdicts, strict);
    for line in &deltas.improvements {
        println!("improvement over the legacy baseline: {}", line);
    }
    println!(
        "{} cases in {:.1} s",
        verdicts.len(),
        started.elapsed().as_secs_f64()
    );
    assert!(
        deltas.problems.is_empty(),
        "engine {} grid {} differs from {}:\n{}\nfull report: {}",
        factory.name(),
        grid,
        baseline_path.display(),
        deltas.problems.join("\n"),
        report_path.display()
    );
}

#[test]
fn a_planted_bad_golden_is_caught() {
    let recording = recording("cursor-movements");

    // No golden at all is a failure, never a silent pass.
    let (outcomes, dumps) = suite::run_vttest_session(&Legacy, &recording, &Err("none".into()));
    assert_eq!(outcomes.len(), recording.screens.len());
    assert!(outcomes.iter().all(|outcome| outcome.failure.is_some()));

    // The engine's own dumps, through the golden files, match.
    let text = dump::render_text_file("vttest/cursor-movements", &keys(&recording), &dumps);
    let attrs = dump::render_attrs_file(&recording.session, &dumps);
    let good = scratch_dir("good-golden");
    write_golden(&good, &recording, &text, &attrs);
    let golden = Golden::load(&good, &recording.session);
    let (outcomes, _) = suite::run_vttest_session(&Legacy, &recording, &golden);
    for outcome in &outcomes {
        assert_eq!(outcome.failure, None, "{}", outcome.id);
    }

    // Plant one wrong character (screen 2), one wrong attribute run
    // (screen 3) and a wrong cursor (screen 4).
    let mut sections = dump::parse_text_file(&text).expect("parse screens");
    let row = &mut sections[2][2 + 4];
    let bar = row.find('|').expect("a row line") + 1;
    let planted = if row[bar..].starts_with('#') {
        "X"
    } else {
        "#"
    };
    row.replace_range(bar..bar + 1, planted);
    sections[4][0] = "cursor 1,1 hidden".to_string();
    let planted_dumps: Vec<ScreenDump> = sections
        .into_iter()
        .zip(&dumps)
        .enumerate()
        .map(|(index, (text, dump))| {
            let mut attrs = dump.attrs.clone();
            if index == 3 {
                attrs.push("{\"row\":24,\"runs\":[[1,1,\"bold\"]]}".to_string());
            }
            ScreenDump { text, attrs }
        })
        .collect();
    let bad = scratch_dir("bad-golden");
    write_golden(
        &bad,
        &recording,
        &dump::render_text_file("vttest/cursor-movements", &keys(&recording), &planted_dumps),
        &dump::render_attrs_file(&recording.session, &planted_dumps),
    );
    let golden = Golden::load(&bad, &recording.session);
    let (outcomes, _) = suite::run_vttest_session(&Legacy, &recording, &golden);
    let failed: Vec<&Outcome> = outcomes
        .iter()
        .filter(|outcome| outcome.failure.is_some())
        .collect();
    let failed_ids: Vec<&str> = failed.iter().map(|outcome| outcome.id.as_str()).collect();
    assert_eq!(
        failed_ids,
        [
            "vttest/cursor-movements/02",
            "vttest/cursor-movements/03",
            "vttest/cursor-movements/04"
        ]
    );
    let reason = |index: usize| failed[index].failure.as_deref().expect("failed");
    assert!(reason(0).contains("expected \"05"), "{}", reason(0));
    assert!(
        reason(1).contains("attrs expected {\"row\":24"),
        "{}",
        reason(1)
    );
    assert!(reason(2).contains("cursor 1,1 hidden"), "{}", reason(2));
}

/// A broken engine that drops every SGR sequence it sees within a chunk.
struct IgnoresSgr;

struct IgnoresSgrEngine {
    terminal: Terminal,
}

fn strip_sgr(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"\x1b[") {
            let params = bytes[i + 2..]
                .iter()
                .take_while(|&&b| b.is_ascii_digit() || b == b';' || b == b':')
                .count();
            if bytes.get(i + 2 + params) == Some(&b'm') {
                i += 3 + params;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

impl Engine for IgnoresSgrEngine {
    fn feed(&mut self, bytes: &[u8]) {
        self.terminal.advance_bytes(strip_sgr(bytes));
    }

    fn snapshot(&self) -> EngineSnapshot {
        snapshot::capture(&self.terminal)
    }

    fn wait_for_replies(&mut self) {
        drain_replies(&mut self.terminal);
    }
}

impl EngineFactory for IgnoresSgr {
    fn name(&self) -> &'static str {
        "mock_ignores_sgr"
    }

    fn build_with(&self, geometry: &Geometry, io: EngineIo) -> Box<dyn Engine> {
        Box::new(IgnoresSgrEngine {
            terminal: new_terminal_with(geometry, io),
        })
    }
}

#[test]
fn a_broken_engine_fails_the_cases_that_cover_its_bug() {
    assert_eq!(strip_sgr(b"a\x1b[1;31mb\x1b[2Jc\x1b[m"), b"ab\x1b[2Jc");

    let sgr_cases: Vec<_> = cases::all()
        .into_iter()
        .filter(|case| case.id.starts_with("sgr/"))
        .collect();
    assert!(sgr_cases.len() >= 20);
    for case in &sgr_cases {
        let broken = suite::run_esctest_case(&IgnoresSgr, case);
        let legacy = suite::run_esctest_case(&Legacy, case);
        // A case that expects only default attributes cannot see the bug;
        // every other one must.
        let resets_only = matches!(
            case.id,
            "sgr/0-resets-everything"
                | "sgr/22-clears-bold-and-faint"
                | "sgr/2x-and-55-clear-their-attributes"
                | "sgr/no-params-resets-everything"
                | "sgr/39-49-default-colors"
        );
        if legacy.failure.is_none() {
            assert_eq!(
                broken.failure.is_some(),
                !resets_only,
                "{}: {:?}",
                broken.id,
                broken.failure
            );
        }
    }

    // The recorded screens with renditions fail too, against the committed
    // goldens, where the legacy engine passes them.
    let fixtures = suite::fixtures_dir();
    let baseline = suite::parse_baseline(
        &std::fs::read_to_string(suite::baseline_path(&fixtures)).expect("read the baseline"),
    )
    .expect("parse the baseline");
    let recording = recording("screen-features");
    let golden = Golden::load(&suite::goldens_dir(&fixtures), &recording.session);
    let (outcomes, _) = suite::run_vttest_session(&IgnoresSgr, &recording, &golden);
    let caught: Vec<&str> = outcomes
        .iter()
        .filter(|outcome| {
            outcome.failure.is_some() && baseline.get(&outcome.id) == Some(&Status::Pass)
        })
        .map(|outcome| outcome.id.as_str())
        .collect();
    assert!(
        !caught.is_empty(),
        "dropping SGR changed no screen that legacy passes"
    );
}

#[test]
fn xfails_are_explicit_justified_and_name_real_cases() {
    let fixtures = suite::fixtures_dir();
    let xfails = suite::load_xfails(&fixtures).unwrap_or_else(|err| panic!("{}", err));
    let ids = every_case_id();
    let unique: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    assert_eq!(unique.len(), ids.len(), "case ids are unique");
    for id in xfails.keys() {
        assert!(unique.contains(id.as_str()), "xfail {} is not a case", id);
    }

    assert!(suite::parse_xfails("esctest/x").is_err(), "no reason");
    assert!(
        suite::parse_xfails("esctest/x short").is_err(),
        "a token reason"
    );
    let twice = "esctest/x a reason that is long enough\nesctest/x a reason that is long enough";
    assert!(suite::parse_xfails(twice).is_err(), "listed twice");

    let planted = suite::parse_xfails(
        "# comment\nesctest/a the engine lacks this feature entirely\n\
         esctest/b the engine lacks this feature entirely\n",
    )
    .expect("well-formed");
    let outcomes = vec![
        Outcome {
            id: "esctest/a".to_string(),
            failure: Some("wrong".to_string()),
        },
        Outcome {
            id: "esctest/b".to_string(),
            failure: None,
        },
        Outcome {
            id: "esctest/c".to_string(),
            failure: Some("wrong".to_string()),
        },
        Outcome {
            id: "esctest/d".to_string(),
            failure: None,
        },
    ];
    let statuses: Vec<Status> = suite::classify(&outcomes, &planted)
        .expect("every entry names a case")
        .iter()
        .map(|verdict| verdict.status)
        .collect();
    assert_eq!(
        statuses,
        [Status::XFail, Status::XPass, Status::Fail, Status::Pass]
    );
    let stale = suite::parse_xfails("esctest/gone the engine lacks this feature entirely")
        .expect("well-formed");
    assert!(suite::classify(&outcomes, &stale).is_err(), "stale entry");
}

#[test]
fn the_recordings_are_real_vttest_sessions() {
    let recordings = recordings();
    assert!(recordings.len() >= 10, "{} recordings", recordings.len());
    let mut screens = 0;
    for recording in &recordings {
        assert_eq!(
            (recording.rows, recording.cols),
            (24, 80),
            "{}",
            recording.session
        );
        let first = &recording.screens[0].bytes;
        // vttest opens with a DA1 query, then draws its main menu.
        assert!(first.starts_with(b"\x1b[0c"), "{}", recording.session);
        let menu = String::from_utf8_lossy(first);
        assert!(
            menu.contains("VT100 test program, version 2.7"),
            "{}",
            recording.session
        );
        assert!(recording.screens.len() >= 8, "{}", recording.session);
        screens += recording.screens.len();
    }
    assert!(screens >= 200, "{} screens", screens);

    let mut bytes = Vec::new();
    vtrec::unescape_into("a\\e[1m\\x41\\\\\\r\\n\\t\\x20", &mut bytes).expect("valid");
    assert_eq!(bytes, b"a\x1b[1mA\\\r\n\t ");
    assert!(vtrec::unescape_into("\\q", &mut Vec::new()).is_err());
    assert!(vtrec::unescape_into("\\x4", &mut Vec::new()).is_err());
    assert!(vtrec::unescape_into("caf\u{e9}", &mut Vec::new()).is_err());
    assert!(vtrec::parse("@session s\n@geometry 1x2.2 24x80\nabc\n").is_err());
}

#[test]
fn goldens_and_the_baseline_cover_every_case_exactly_once() {
    let fixtures = suite::fixtures_dir();
    for recording in recordings() {
        let golden = Golden::load(&suite::goldens_dir(&fixtures), &recording.session)
            .unwrap_or_else(|err| panic!("{}", err));
        assert_eq!(
            golden.text.len(),
            recording.screens.len(),
            "{}",
            recording.session
        );
    }
    let baseline = suite::parse_baseline(
        &std::fs::read_to_string(suite::baseline_path(&fixtures)).expect("read the baseline"),
    )
    .unwrap_or_else(|err| panic!("{}", err));
    let xfails = suite::load_xfails(&fixtures).unwrap_or_else(|err| panic!("{}", err));
    let ids = every_case_id();
    let listed: BTreeSet<&str> = baseline.keys().map(String::as_str).collect();
    let cases: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    assert_eq!(listed, cases, "the baseline lists exactly the cases");
    for (id, status) in &baseline {
        assert_eq!(
            *status == Status::XFail,
            xfails.contains_key(id),
            "{}: XFAIL in the baseline exactly when xfail.txt lists it",
            id
        );
    }
}

#[test]
fn the_attribute_file_format_round_trips() {
    let dumps = vec![
        ScreenDump {
            text: vec!["cursor 1,1 visible".to_string()],
            attrs: Vec::new(),
        },
        ScreenDump {
            text: vec!["cursor 2,3 hidden".to_string()],
            attrs: vec![
                "{\"row\":1,\"runs\":[[1,4,\"bold\"]]}".to_string(),
                "{\"row\":2,\"runs\":[[3,2,\"italic\"],[9,1,\"fg=1\"]]}".to_string(),
            ],
        },
    ];
    let text = dump::render_attrs_file("s\"x", &dumps);
    assert!(text.starts_with("{\"session\":\"s\\\"x\",\"screens\":[\n"));
    let parsed = dump::parse_attrs_file(&text).expect("parse");
    assert_eq!(parsed, vec![Vec::new(), dumps[1].attrs.clone()]);
    assert!(dump::parse_attrs_file(&text.replace("]}\n]}\n", "]}\n")).is_err());
    assert_eq!(dump::json_string("a\\b\u{1}"), "\"a\\\\b\\u0001\"");

    let keys = vec!["''".to_string(), "'\\r'".to_string()];
    let screens =
        dump::parse_text_file(&dump::render_text_file("vttest/s", &keys, &dumps)).expect("parse");
    assert_eq!(screens, vec![dumps[0].text.clone(), dumps[1].text.clone()]);
}
