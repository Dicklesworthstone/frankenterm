use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const RUSTFMT_TARGETS: &[&str] = &[
    "crates/frankenterm-core/src/agent_config_templates.rs",
    "crates/frankenterm-core/src/agent_correlator.rs",
    "crates/frankenterm-core/src/agent_provider.rs",
    "crates/frankenterm-core/src/session_resume.rs",
    "crates/frankenterm-core/tests/agent_inventory_golden.rs",
    "crates/frankenterm-core/tests/agent_provider_bridge_integration.rs",
    "crates/frankenterm-core/tests/e2e_antigravity_session_resume_script.rs",
    "crates/frankenterm-core/tests/golden_metamorphic_incident_recorder.rs",
    "crates/frankenterm-core/tests/integration_agent_detection.rs",
    "crates/frankenterm-core/tests/proptest_agent_config_templates.rs",
    "crates/frankenterm-core/tests/proptest_session_resume.rs",
];

#[test]
fn antigravity_owned_rust_files_are_rustfmt_clean() {
    let repo_root = repo_root();
    let missing = RUSTFMT_TARGETS
        .iter()
        .filter(|path| !repo_root.join(path).is_file())
        .copied()
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "rustfmt proof target(s) missing from repo root {}: {missing:?}",
        repo_root.display()
    );

    let mut failures = Vec::new();
    for rustfmt in rustfmt_commands() {
        match rustfmt.run(&repo_root) {
            Ok(output) if output.status.success() => return,
            Ok(output) => failures.push(format!(
                "command: {}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
                rustfmt.label,
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )),
            Err(err) => failures.push(format!("command: {}\nerror: {err}", rustfmt.label)),
        }
    }

    panic!(
        "Antigravity-owned Rust files are not rustfmt-clean, or no usable rustfmt was found.\n\n{}",
        failures.join("\n\n")
    );
}

fn repo_root() -> PathBuf {
    manifest_dir()
        .parent()
        .and_then(Path::parent)
        .expect("frankenterm-core manifest dir should live under crates/frankenterm-core")
        .to_path_buf()
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rustfmt_commands() -> Vec<RustfmtCommand> {
    let mut commands = Vec::new();
    if let Some(path) = env::var_os("RUSTFMT") {
        commands.push(RustfmtCommand::new(
            path,
            std::iter::empty::<&str>(),
            "$RUSTFMT",
        ));
    }

    commands.push(RustfmtCommand::new(
        "rustfmt",
        std::iter::empty::<&str>(),
        "rustfmt",
    ));
    commands.push(RustfmtCommand::new(
        "rustup",
        ["run", "stable", "rustfmt"],
        "rustup run stable rustfmt",
    ));
    commands.push(RustfmtCommand::new(
        "rustup",
        ["run", "nightly", "rustfmt"],
        "rustup run nightly rustfmt",
    ));
    commands
}

struct RustfmtCommand {
    program: OsString,
    prefix_args: Vec<OsString>,
    label: String,
}

impl RustfmtCommand {
    fn new(
        program: impl Into<OsString>,
        prefix_args: impl IntoIterator<Item = impl Into<OsString>>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            prefix_args: prefix_args.into_iter().map(Into::into).collect(),
            label: label.into(),
        }
    }

    fn run(&self, repo_root: &Path) -> std::io::Result<std::process::Output> {
        Command::new(&self.program)
            .current_dir(repo_root)
            .args(&self.prefix_args)
            .args(["--edition", "2024", "--check"])
            .args(RUSTFMT_TARGETS)
            .output()
    }
}
