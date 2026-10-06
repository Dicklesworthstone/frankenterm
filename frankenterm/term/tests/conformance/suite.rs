//! The runner: every case, its outcome on one engine, explicit xfails, the
//! report, and the comparison with the committed baseline.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use super::cases;
use super::dump::{self, ScreenDump};
use super::esctest::{EscCase, Session};
use super::vtrec::{self, Recording};
use crate::differential::engine::{candidates, EngineFactory, Geometry, Legacy};

/// `fixtures/conformance` at the workspace root.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/conformance")
}

pub fn recordings_dir(fixtures: &Path) -> PathBuf {
    fixtures.join("vttest")
}

pub fn goldens_dir(fixtures: &Path) -> PathBuf {
    fixtures.join("vttest").join("goldens")
}

pub fn baseline_path(fixtures: &Path) -> PathBuf {
    fixtures.join("baseline").join("legacy.txt")
}

pub fn xfail_path(fixtures: &Path) -> PathBuf {
    fixtures.join("xfail.txt")
}

/// The engine named by `FT_CONFORMANCE_ENGINE` (default `legacy`): the
/// oracle or any candidate registered with the differential harness.
pub fn selected_engine() -> Result<Box<dyn EngineFactory>, String> {
    let wanted = std::env::var("FT_CONFORMANCE_ENGINE").unwrap_or_else(|_| "legacy".to_string());
    let mut engines: Vec<Box<dyn EngineFactory>> = vec![Box::new(Legacy)];
    engines.extend(candidates());
    let names: Vec<&'static str> = engines.iter().map(|engine| engine.name()).collect();
    engines
        .into_iter()
        .find(|engine| engine.name() == wanted)
        .ok_or_else(|| format!("FT_CONFORMANCE_ENGINE={} is not one of {:?}", wanted, names))
}

/// The grid engine `Terminal::new` selects (ft-yccm0.3.3.4 reads
/// `FT_GRID_ENGINE`; until then every terminal uses the legacy grid).
pub fn grid_engine() -> String {
    std::env::var("FT_GRID_ENGINE").unwrap_or_else(|_| "legacy".to_string())
}

pub struct Outcome {
    pub id: String,
    pub failure: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// Failed, and listed in xfail.txt.
    XFail,
    /// Listed in xfail.txt, but passed.
    XPass,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::XFail => "XFAIL",
            Status::XPass => "XPASS",
        }
    }

    pub fn parse(label: &str) -> Option<Status> {
        match label {
            "PASS" => Some(Status::Pass),
            "FAIL" => Some(Status::Fail),
            "XFAIL" => Some(Status::XFail),
            "XPASS" => Some(Status::XPass),
            _ => None,
        }
    }

    fn passed(self) -> bool {
        matches!(self, Status::Pass | Status::XPass)
    }
}

pub struct Verdict {
    pub id: String,
    pub status: Status,
    pub detail: String,
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

pub fn vttest_id(session: &str, screen: usize) -> String {
    format!("vttest/{}/{:02}", session, screen)
}

pub fn esctest_id(case: &EscCase) -> String {
    format!("esctest/{}", case.id)
}

/// A session's golden: per-screen text sections and attribute rows.
pub struct Golden {
    pub text: Vec<Vec<String>>,
    pub attrs: Vec<Vec<String>>,
}

impl Golden {
    pub fn load(dir: &Path, session: &str) -> Result<Golden, String> {
        let read = |name: String| {
            let path = dir.join(&name);
            std::fs::read_to_string(&path).map_err(|err| format!("{}: {}", path.display(), err))
        };
        let text = dump::parse_text_file(&read(format!("{}.screens.txt", session))?)
            .map_err(|err| format!("{}.screens.txt: {}", session, err))?;
        let attrs = dump::parse_attrs_file(&read(format!("{}.attrs.json", session))?)
            .map_err(|err| format!("{}.attrs.json: {}", session, err))?;
        if text.len() != attrs.len() {
            return Err(format!(
                "{}: {} text sections but {} attribute sections",
                session,
                text.len(),
                attrs.len()
            ));
        }
        Ok(Golden { text, attrs })
    }

    pub fn screen(&self, index: usize) -> Option<ScreenDump> {
        Some(ScreenDump {
            text: self.text.get(index)?.clone(),
            attrs: self.attrs.get(index)?.clone(),
        })
    }
}

/// Replays one recording screen by screen and compares each screen with
/// its golden. Returns the outcomes and the dumps the engine produced.
pub fn run_vttest_session(
    factory: &dyn EngineFactory,
    recording: &Recording,
    golden: &Result<Golden, String>,
) -> (Vec<Outcome>, Vec<ScreenDump>) {
    let geometry = Geometry {
        rows: recording.rows,
        cols: recording.cols,
        scrollback: 32,
    };
    let mut engine = factory.build(&geometry);
    let mut outcomes = Vec::new();
    let mut dumps = Vec::new();
    let mut crashed: Option<String> = None;
    for (index, screen) in recording.screens.iter().enumerate() {
        let id = vttest_id(&recording.session, index);
        if let Some(message) = &crashed {
            outcomes.push(Outcome {
                id,
                failure: Some(format!(
                    "the engine panicked on an earlier screen: {}",
                    message
                )),
            });
            continue;
        }
        let rows = recording.rows;
        let result = catch_unwind(AssertUnwindSafe(|| {
            engine.feed(&screen.bytes);
            dump::dump(&engine.snapshot(), rows)
        }));
        let actual = match result {
            Ok(actual) => actual,
            Err(payload) => {
                let message = panic_message(payload);
                outcomes.push(Outcome {
                    id,
                    failure: Some(format!("the engine panicked: {}", message)),
                });
                crashed = Some(message);
                continue;
            }
        };
        let failure = match golden {
            Err(err) => Some(format!("no golden: {}", err)),
            Ok(golden) if golden.text.len() != recording.screens.len() => Some(format!(
                "the golden has {} screens, the recording {}",
                golden.text.len(),
                recording.screens.len()
            )),
            Ok(golden) => {
                let expected = golden.screen(index).expect("counts checked above");
                dump::compare(&expected, &actual)
            }
        };
        outcomes.push(Outcome { id, failure });
        dumps.push(actual);
    }
    (outcomes, dumps)
}

pub fn run_esctest_case(factory: &dyn EngineFactory, case: &EscCase) -> Outcome {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut session = Session::new(factory, case.rows, case.cols);
        (case.run)(&mut session)
    }));
    let failure = match result {
        Ok(Ok(())) => None,
        Ok(Err(failure)) => Some(format!("{} [{}]", failure, case.reference)),
        Err(payload) => Some(format!("the engine panicked: {}", panic_message(payload))),
    };
    Outcome {
        id: esctest_id(case),
        failure,
    }
}

/// Everything one run produced.
pub struct SuiteRun {
    pub outcomes: Vec<Outcome>,
    /// Per recording: session name, recorded keys, and the engine's dumps.
    pub sessions: Vec<(String, Vec<String>, Vec<ScreenDump>)>,
}

pub fn run_suite(factory: &dyn EngineFactory, fixtures: &Path) -> Result<SuiteRun, String> {
    let recordings = vtrec::load_dir(&recordings_dir(fixtures))?;
    let goldens = goldens_dir(fixtures);
    let mut outcomes = Vec::new();
    let mut sessions = Vec::new();
    for recording in &recordings {
        let golden = Golden::load(&goldens, &recording.session);
        let (session_outcomes, dumps) = run_vttest_session(factory, recording, &golden);
        outcomes.extend(session_outcomes);
        let keys = recording
            .screens
            .iter()
            .map(|screen| screen.key.clone())
            .collect();
        sessions.push((recording.session.clone(), keys, dumps));
    }
    for case in cases::all() {
        outcomes.push(run_esctest_case(factory, &case));
    }
    let mut seen = BTreeSet::new();
    for outcome in &outcomes {
        if !seen.insert(outcome.id.as_str()) {
            return Err(format!("duplicate case id {}", outcome.id));
        }
    }
    Ok(SuiteRun { outcomes, sessions })
}

/// `id reason` lines; `#` lines and blank lines are ignored. Every entry
/// must give a reason.
pub fn parse_xfails(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut xfails = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (id, reason) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("xfail line {}: {} has no reason", index + 1, line))?;
        let reason = reason.trim();
        if reason.len() < 20 {
            return Err(format!(
                "xfail line {}: the reason for {} is too short to justify it",
                index + 1,
                id
            ));
        }
        if xfails.insert(id.to_string(), reason.to_string()).is_some() {
            return Err(format!("xfail line {}: {} is listed twice", index + 1, id));
        }
    }
    Ok(xfails)
}

pub fn load_xfails(fixtures: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = xfail_path(fixtures);
    let text =
        std::fs::read_to_string(&path).map_err(|err| format!("{}: {}", path.display(), err))?;
    parse_xfails(&text)
}

/// Applies the xfail list. An xfail entry that names no case is an error,
/// so the list cannot rot.
pub fn classify(
    outcomes: &[Outcome],
    xfails: &BTreeMap<String, String>,
) -> Result<Vec<Verdict>, String> {
    let ids: BTreeSet<&str> = outcomes.iter().map(|outcome| outcome.id.as_str()).collect();
    for id in xfails.keys() {
        if !ids.contains(id.as_str()) {
            return Err(format!("xfail.txt names {}, which is not a case", id));
        }
    }
    Ok(outcomes
        .iter()
        .map(|outcome| {
            let reason = xfails.get(&outcome.id);
            let (status, detail) = match (&outcome.failure, reason) {
                (None, None) => (Status::Pass, String::new()),
                (Some(failure), None) => (Status::Fail, failure.clone()),
                (Some(failure), Some(reason)) => {
                    (Status::XFail, format!("xfail: {}\n{}", reason, failure))
                }
                (None, Some(reason)) => (
                    Status::XPass,
                    format!("listed as xfail but passes; remove the entry: {}", reason),
                ),
            };
            Verdict {
                id: outcome.id.clone(),
                status,
                detail,
            }
        })
        .collect())
}

pub fn count(verdicts: &[Verdict], status: Status) -> usize {
    verdicts
        .iter()
        .filter(|verdict| verdict.status == status)
        .count()
}

/// The human-readable report: totals, then one line per case with the
/// reason under every case that did not plainly pass.
pub fn render_report(engine: &str, grid: &str, verdicts: &[Verdict]) -> String {
    let mut out = format!(
        "conformance report: engine {} grid {}\n{} cases: {} pass, {} fail, {} xfail, {} xpass\n",
        engine,
        grid,
        verdicts.len(),
        count(verdicts, Status::Pass),
        count(verdicts, Status::Fail),
        count(verdicts, Status::XFail),
        count(verdicts, Status::XPass)
    );
    for verdict in verdicts {
        let _ = writeln!(out, "{:<5} {}", verdict.status.label(), verdict.id);
        for line in verdict.detail.lines() {
            let _ = writeln!(out, "      {}", line);
        }
    }
    out
}

/// The committed baseline: one `STATUS id` line per case.
pub fn render_baseline(verdicts: &[Verdict]) -> String {
    let mut out = String::from(
        "# Conformance baseline (ft-yccm0.1.9): the legacy engine (FT_CONFORMANCE_ENGINE=legacy,\n\
         # FT_GRID_ENGINE unset). One line per case. A change needs a recorded reason in the\n\
         # commit body; see fixtures/conformance/README.md.\n",
    );
    for verdict in verdicts {
        let _ = writeln!(out, "{} {}", verdict.status.label(), verdict.id);
    }
    out
}

pub fn parse_baseline(text: &str) -> Result<BTreeMap<String, Status>, String> {
    let mut baseline = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (label, id) = line
            .split_once(' ')
            .ok_or_else(|| format!("baseline line {}: expected STATUS id", index + 1))?;
        let status = Status::parse(label)
            .ok_or_else(|| format!("baseline line {}: unknown status {}", index + 1, label))?;
        if status == Status::Fail || status == Status::XPass {
            return Err(format!(
                "baseline line {}: {} {} must be fixed or explained in xfail.txt first",
                index + 1,
                label,
                id
            ));
        }
        if baseline.insert(id.to_string(), status).is_some() {
            return Err(format!(
                "baseline line {}: {} is listed twice",
                index + 1,
                id
            ));
        }
    }
    Ok(baseline)
}

pub struct Deltas {
    /// Differences that fail the run.
    pub problems: Vec<String>,
    /// Cases a candidate fixed.
    pub improvements: Vec<String>,
}

/// Compares a run with the baseline. `strict` (the legacy engine itself)
/// requires the exact baseline; otherwise only regressions are problems.
pub fn compare_with_baseline(
    baseline: &BTreeMap<String, Status>,
    verdicts: &[Verdict],
    strict: bool,
) -> Deltas {
    let mut problems = Vec::new();
    let mut improvements = Vec::new();
    let mut current = BTreeMap::new();
    for verdict in verdicts {
        current.insert(verdict.id.as_str(), verdict.status);
        if verdict.status == Status::Fail {
            problems.push(format!(
                "FAIL {}: {}",
                verdict.id,
                first_line(&verdict.detail)
            ));
        }
        match baseline.get(&verdict.id) {
            None => problems.push(format!(
                "{} is not in the baseline; add it with its status",
                verdict.id
            )),
            Some(&expected) if expected == verdict.status => {}
            Some(&expected) => {
                let line = format!(
                    "{}: baseline {} now {}",
                    verdict.id,
                    expected.label(),
                    verdict.status.label()
                );
                if strict || (expected.passed() && !verdict.status.passed()) {
                    problems.push(line);
                } else if !expected.passed() && verdict.status.passed() {
                    improvements.push(line);
                }
            }
        }
    }
    for id in baseline.keys() {
        if !current.contains_key(id.as_str()) {
            problems.push(format!("{} is in the baseline but is no longer a case", id));
        }
    }
    Deltas {
        problems,
        improvements,
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}
