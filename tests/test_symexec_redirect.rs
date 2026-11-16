use frankenterm_core::ars_symbolic_exec::*;
use frankenterm_core::mdl_extraction::CommandBlock;

fn make_cmd(index: u32, command: &str) -> CommandBlock {
    CommandBlock {
        index,
        command: command.to_string(),
        exit_code: Some(0),
        duration_us: Some(1000),
        output_preview: None,
        timestamp_us: (index as u64 + 1) * 1_000_000,
    }
}

fn cwd_executor(cwd: &str) -> SymbolicExecutor {
    SymbolicExecutor::with_cwd(cwd)
}

fn main() {
    let exec = cwd_executor("/home/user/project");
    let cmds = vec![make_cmd(0, "echo evil > /etc/passwd")];
    let verdict = exec.analyze(&cmds);
    assert!(verdict.is_unsafe());
    if let SafetyVerdict::Unsafe(v) = &verdict {
        assert!(v.violations.iter().any(|v| v.category == ViolationCategory::PathTraversal));
    } else {
        panic!("Expected Unsafe");
    }
}
