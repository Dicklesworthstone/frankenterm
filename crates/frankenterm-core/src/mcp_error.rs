//! MCP error code definitions and error mapping utilities.

use crate::cass::CassError;
use crate::caut::CautError;
use crate::error::{Error, StorageError, WeztermError};

pub(crate) const MCP_ERR_INVALID_ARGS: &str = "FT-MCP-0001";
pub(crate) const MCP_ERR_CONFIG: &str = "FT-MCP-0003";
pub(crate) const MCP_ERR_WEZTERM: &str = "FT-MCP-0004";
pub(crate) const MCP_ERR_STORAGE: &str = "FT-MCP-0005";
pub(crate) const MCP_ERR_POLICY: &str = "FT-MCP-0006";
pub(crate) const MCP_ERR_PANE_NOT_FOUND: &str = "FT-MCP-0007";
pub(crate) const MCP_ERR_WORKFLOW: &str = "FT-MCP-0008";
pub(crate) const MCP_ERR_TIMEOUT: &str = "FT-MCP-0009";
pub(crate) const MCP_ERR_NOT_IMPLEMENTED: &str = "FT-MCP-0010";
pub(crate) const MCP_ERR_FTS_QUERY: &str = "FT-MCP-0011";
pub(crate) const MCP_ERR_RESERVATION_CONFLICT: &str = "FT-MCP-0012";
pub(crate) const MCP_ERR_CAUT: &str = "FT-MCP-0013";
pub(crate) const MCP_ERR_CASS: &str = "FT-MCP-0014";
pub(crate) const MCP_ERR_REMOTE_TEXT_UNAVAILABLE: &str = "FT-MCP-0015";
pub(crate) const MCP_ERR_CURSOR_DISCONTINUITY: &str = "FT-MCP-0016";
pub(crate) const MCP_ERR_INDETERMINATE_EFFECT: &str = "FT-MCP-0017";
pub(crate) const MCP_ERR_INTERNAL: &str = "FT-MCP-9000";

#[derive(Debug)]
pub(crate) struct McpToolError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) hint: Option<String>,
}

impl McpToolError {
    pub(crate) fn new(code: &'static str, message: String, hint: Option<String>) -> Self {
        Self {
            code,
            message,
            hint,
        }
    }

    pub(crate) fn from_error(err: Error) -> Self {
        let (code, hint) = map_mcp_error(&err);
        Self {
            code,
            message: redacted_mcp_error_message(&err, code),
            hint,
        }
    }

    pub(crate) fn from_caut_error(err: CautError) -> Self {
        let (code, hint) = map_caut_error(&err);
        Self {
            code,
            message: classified_auxiliary_mcp_error_message("caut", code),
            hint,
        }
    }

    pub(crate) fn from_cass_error(err: CassError) -> Self {
        let (code, hint) = map_cass_error(&err);
        Self {
            code,
            message: classified_auxiliary_mcp_error_message("cass", code),
            hint,
        }
    }
}

fn classified_auxiliary_mcp_error_message(tool: &'static str, code: &'static str) -> String {
    match code {
        MCP_ERR_CONFIG => format!("{tool} is unavailable"),
        MCP_ERR_TIMEOUT => format!("{tool} request timed out"),
        _ => format!("{tool} request failed"),
    }
}

fn redacted_mcp_error_message(error: &Error, code: &'static str) -> String {
    // Classify before rendering. Calling `Display` first can allocate and copy
    // an arbitrarily large backend/storage/policy string even when the generic
    // MCP envelope later redacts or truncates it.
    match code {
        MCP_ERR_PANE_NOT_FOUND => match error {
            Error::Wezterm(WeztermError::PaneNotFound(pane_id)) => {
                format!("Pane not found: {pane_id}")
            }
            _ => "Pane not found".to_string(),
        },
        MCP_ERR_TIMEOUT => "Request timed out or was cancelled".to_string(),
        MCP_ERR_WEZTERM => "Backend bridge request failed".to_string(),
        MCP_ERR_STORAGE => "Storage unavailable".to_string(),
        MCP_ERR_INDETERMINATE_EFFECT => "Operation outcome is indeterminate".to_string(),
        MCP_ERR_FTS_QUERY => "Search query rejected".to_string(),
        MCP_ERR_RESERVATION_CONFLICT => "Reservation conflict".to_string(),
        MCP_ERR_CONFIG => "Configuration unavailable".to_string(),
        MCP_ERR_WORKFLOW => "Workflow request failed".to_string(),
        MCP_ERR_POLICY => "Policy rejected the request".to_string(),
        MCP_ERR_NOT_IMPLEMENTED => "MCP surface unavailable".to_string(),
        _ => "Internal error".to_string(),
    }
}

pub(crate) fn map_caut_error(error: &CautError) -> (&'static str, Option<String>) {
    match error {
        CautError::NotInstalled => (
            MCP_ERR_CONFIG,
            Some("Install caut and ensure it is on PATH.".to_string()),
        ),
        CautError::Timeout { .. } => (
            MCP_ERR_TIMEOUT,
            Some("Retry the refresh or increase caut timeout.".to_string()),
        ),
        CautError::NonZeroExit { .. } => (
            MCP_ERR_CAUT,
            Some("Check caut authentication and logs, then retry.".to_string()),
        ),
        CautError::OutputTooLarge { .. } => (
            MCP_ERR_CAUT,
            Some("Reduce the caut account set or output size, then retry.".to_string()),
        ),
        CautError::InvalidJson { .. } => (
            MCP_ERR_CAUT,
            Some("Upgrade caut or verify its JSON output format.".to_string()),
        ),
        CautError::Io { .. } => (
            MCP_ERR_CAUT,
            Some("Check caut binary permissions and retry.".to_string()),
        ),
    }
}

pub(crate) fn map_cass_error(error: &CassError) -> (&'static str, Option<String>) {
    match error {
        CassError::NotInstalled => (
            MCP_ERR_CONFIG,
            Some("Install cass and ensure it is on PATH.".to_string()),
        ),
        CassError::Timeout { .. } => (
            MCP_ERR_TIMEOUT,
            Some("Retry the query or increase cass timeout.".to_string()),
        ),
        CassError::NonZeroExit { .. } => (
            MCP_ERR_CASS,
            Some("Check cass status and diagnostics, then retry.".to_string()),
        ),
        CassError::OutputTooLarge { .. } => (
            MCP_ERR_CASS,
            Some("Reduce the result limit or request minimal fields.".to_string()),
        ),
        CassError::InvalidJson { .. } => (
            MCP_ERR_CASS,
            Some("Upgrade cass or verify its JSON output format.".to_string()),
        ),
        CassError::NoResults { .. } => (
            MCP_ERR_CASS,
            Some("Broaden the search query or verify the cass index.".to_string()),
        ),
        CassError::Io { .. } => (
            MCP_ERR_CASS,
            Some("Check cass binary permissions and retry.".to_string()),
        ),
    }
}

pub(crate) fn map_mcp_error(error: &Error) -> (&'static str, Option<String>) {
    match error {
        Error::Wezterm(WeztermError::PaneNotFound(_)) => (
            MCP_ERR_PANE_NOT_FOUND,
            Some("Use wa.state to list available panes.".to_string()),
        ),
        Error::Wezterm(WeztermError::Timeout(_)) => (
            MCP_ERR_TIMEOUT,
            Some(
                "Increase timeout or ensure the active backend bridge (current: WezTerm) is responsive."
                    .to_string(),
            ),
        ),
        Error::Wezterm(WeztermError::NotRunning) => (
            MCP_ERR_WEZTERM,
            Some("Is the active backend bridge (current: WezTerm) running?".to_string()),
        ),
        Error::Wezterm(WeztermError::CliNotFound) => (
            MCP_ERR_WEZTERM,
            Some(
                "Install/configure the active backend bridge (current: WezTerm) and ensure it is in PATH."
                    .to_string(),
            ),
        ),
        Error::Wezterm(WeztermError::IndeterminateMutation { .. }) => (
            MCP_ERR_INDETERMINATE_EFFECT,
            Some(
                "The mux mutation may already have taken effect. Reconcile live state and do not retry automatically."
                    .to_string(),
            ),
        ),
        Error::Wezterm(_) => (MCP_ERR_WEZTERM, None),
        Error::Config(_) => (MCP_ERR_CONFIG, None),
        Error::Storage(StorageError::ReservationConflict { .. }) => (
            MCP_ERR_RESERVATION_CONFLICT,
            Some("Use wa.reservations to inspect or release the conflicting reservation.".to_string()),
        ),
        // ft-kccj8: an FTS query the lint accepted but SQLite rejected is a
        // CALLER error, not a DB outage — surfacing it as the redacted
        // "Storage unavailable" (FT-MCP-0005) made it indistinguishable from
        // a real outage and unactionable (e.g. hyphenated barewords parse as
        // FTS5 column-filter negation: "no such column").
        Error::Storage(StorageError::FtsQueryError(_)) => (
            MCP_ERR_FTS_QUERY,
            Some(
                "Quote the query as a phrase (\"like-this\") or remove FTS5 operator characters (- : * ( ) NEAR/AND/OR/NOT).".to_string(),
            ),
        ),
        Error::Storage(
            StorageError::IndeterminateMutation { .. }
            | StorageError::WriterSettlementIndeterminate { .. },
        ) => (
            MCP_ERR_INDETERMINATE_EFFECT,
            Some(
                "The storage effect may already be durable. Reconcile stored state and do not retry automatically."
                    .to_string(),
            ),
        ),
        Error::Storage(_) => (MCP_ERR_STORAGE, None),
        Error::Workflow(_) => (MCP_ERR_WORKFLOW, None),
        Error::Policy(_) => (MCP_ERR_POLICY, None),
        Error::Cancelled(_) => (
            MCP_ERR_TIMEOUT,
            Some("Retry the request or increase its timeout budget.".to_string()),
        ),
        Error::RuntimeOperation {
            operation: "mcp_bridge.build_server_with_db",
            ..
        } => (
            MCP_ERR_NOT_IMPLEMENTED,
            Some(
                "Supply a database path or call build_server_degraded(config) to opt into the stripped no-db catalog."
                    .to_string(),
            ),
        ),
        _ => (MCP_ERR_INTERNAL, None),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MCP_ERR_CASS, MCP_ERR_CAUT, MCP_ERR_CONFIG, MCP_ERR_CURSOR_DISCONTINUITY,
        MCP_ERR_FTS_QUERY, MCP_ERR_INDETERMINATE_EFFECT, MCP_ERR_INTERNAL, MCP_ERR_INVALID_ARGS, MCP_ERR_NOT_IMPLEMENTED,
        MCP_ERR_PANE_NOT_FOUND, MCP_ERR_POLICY, MCP_ERR_REMOTE_TEXT_UNAVAILABLE,
        MCP_ERR_RESERVATION_CONFLICT, MCP_ERR_STORAGE, MCP_ERR_TIMEOUT, MCP_ERR_WEZTERM,
        MCP_ERR_WORKFLOW, McpToolError, map_cass_error, map_caut_error, map_mcp_error,
    };
    use crate::cass::CassError;
    use crate::caut::CautError;
    use crate::error::{Error, StorageError, WeztermError};
    use proptest::prelude::*;

    // ========================================================================
    // Error Code Constants
    // ========================================================================

    #[test]
    fn error_codes_are_unique() {
        let codes = [
            MCP_ERR_INVALID_ARGS,
            MCP_ERR_CONFIG,
            MCP_ERR_WEZTERM,
            MCP_ERR_STORAGE,
            MCP_ERR_POLICY,
            MCP_ERR_PANE_NOT_FOUND,
            MCP_ERR_WORKFLOW,
            MCP_ERR_TIMEOUT,
            MCP_ERR_NOT_IMPLEMENTED,
            MCP_ERR_FTS_QUERY,
            MCP_ERR_RESERVATION_CONFLICT,
            MCP_ERR_CAUT,
            MCP_ERR_CASS,
            MCP_ERR_REMOTE_TEXT_UNAVAILABLE,
            MCP_ERR_CURSOR_DISCONTINUITY,
            MCP_ERR_INDETERMINATE_EFFECT,
            MCP_ERR_INTERNAL,
        ];
        let mut seen = std::collections::HashSet::new();
        for code in codes {
            assert!(seen.insert(code), "Duplicate error code: {code}");
        }
    }

    #[test]
    fn error_codes_have_ft_mcp_prefix() {
        let codes = [
            MCP_ERR_INVALID_ARGS,
            MCP_ERR_CONFIG,
            MCP_ERR_WEZTERM,
            MCP_ERR_STORAGE,
            MCP_ERR_POLICY,
            MCP_ERR_PANE_NOT_FOUND,
            MCP_ERR_WORKFLOW,
            MCP_ERR_TIMEOUT,
            MCP_ERR_NOT_IMPLEMENTED,
            MCP_ERR_FTS_QUERY,
            MCP_ERR_RESERVATION_CONFLICT,
            MCP_ERR_CAUT,
            MCP_ERR_CASS,
            MCP_ERR_REMOTE_TEXT_UNAVAILABLE,
            MCP_ERR_CURSOR_DISCONTINUITY,
            MCP_ERR_INDETERMINATE_EFFECT,
            MCP_ERR_INTERNAL,
        ];
        for code in codes {
            assert!(
                code.starts_with("FT-MCP-"),
                "Code {code} missing FT-MCP- prefix"
            );
        }
    }

    #[test]
    fn error_codes_match_published_schema_assignments() {
        let assignments = [
            (MCP_ERR_INVALID_ARGS, "FT-MCP-0001"),
            (MCP_ERR_CONFIG, "FT-MCP-0003"),
            (MCP_ERR_WEZTERM, "FT-MCP-0004"),
            (MCP_ERR_STORAGE, "FT-MCP-0005"),
            (MCP_ERR_POLICY, "FT-MCP-0006"),
            (MCP_ERR_PANE_NOT_FOUND, "FT-MCP-0007"),
            (MCP_ERR_WORKFLOW, "FT-MCP-0008"),
            (MCP_ERR_TIMEOUT, "FT-MCP-0009"),
            (MCP_ERR_NOT_IMPLEMENTED, "FT-MCP-0010"),
            (MCP_ERR_FTS_QUERY, "FT-MCP-0011"),
            (MCP_ERR_RESERVATION_CONFLICT, "FT-MCP-0012"),
            (MCP_ERR_CAUT, "FT-MCP-0013"),
            (MCP_ERR_CASS, "FT-MCP-0014"),
            (MCP_ERR_REMOTE_TEXT_UNAVAILABLE, "FT-MCP-0015"),
            (MCP_ERR_CURSOR_DISCONTINUITY, "FT-MCP-0016"),
            (MCP_ERR_INDETERMINATE_EFFECT, "FT-MCP-0017"),
            (MCP_ERR_INTERNAL, "FT-MCP-9000"),
        ];

        for (actual, expected) in assignments {
            assert_eq!(actual, expected);
        }
    }

    // ========================================================================
    // McpToolError Construction
    // ========================================================================

    #[test]
    fn mcp_tool_error_new() {
        let err = McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            "bad args".to_string(),
            Some("fix it".to_string()),
        );
        assert_eq!(err.code, MCP_ERR_INVALID_ARGS);
        assert_eq!(err.message, "bad args");
        assert_eq!(err.hint.as_deref(), Some("fix it"));
    }

    #[test]
    fn mcp_tool_error_new_no_hint() {
        let err = McpToolError::new(MCP_ERR_STORAGE, "db error".to_string(), None);
        assert_eq!(err.code, MCP_ERR_STORAGE);
        assert!(err.hint.is_none());
    }

    #[test]
    fn mcp_tool_error_from_error_wezterm_not_running() {
        let err = Error::Wezterm(WeztermError::NotRunning);
        let mcp_err = McpToolError::from_error(err);
        assert_eq!(mcp_err.code, MCP_ERR_WEZTERM);
        assert!(mcp_err.hint.is_some());
    }

    #[test]
    fn mcp_tool_error_from_error_pane_not_found() {
        let err = Error::Wezterm(WeztermError::PaneNotFound(42));
        let mcp_err = McpToolError::from_error(err);
        assert_eq!(mcp_err.code, MCP_ERR_PANE_NOT_FOUND);
        assert!(mcp_err.hint.is_some());
    }

    #[test]
    fn mcp_tool_error_from_error_redacts_storage_and_runtime_details() {
        let storage = Error::Storage(crate::error::StorageError::Database(
            "sqlite busy /tmp/secret.db".into(),
        ));
        let storage_err = McpToolError::from_error(storage);
        assert_eq!(storage_err.code, MCP_ERR_STORAGE);
        assert_eq!(storage_err.message, "Storage unavailable");
        assert!(!storage_err.message.contains("/tmp/secret.db"));

        let runtime =
            Error::runtime_backend("mcp test runtime", "tokio worker panic: internal detail");
        let runtime_err = McpToolError::from_error(runtime);
        assert_eq!(runtime_err.code, MCP_ERR_INTERNAL);
        assert_eq!(runtime_err.message, "Internal error");
        assert!(!runtime_err.message.contains("internal detail"));

        // Every variant routed to MCP_ERR_INTERNAL via the wildcard in
        // map_mcp_error must also be redacted — especially Io which leaks fs paths.
        let io = Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file: /Users/x/.ssh/id_rsa",
        ));
        let io_err = McpToolError::from_error(io);
        assert_eq!(io_err.code, MCP_ERR_INTERNAL);
        assert_eq!(io_err.message, "Internal error");
        assert!(!io_err.message.contains("id_rsa"));

        let secret = format!("backend-secret-{}", "x".repeat(128 * 1024));
        let wezterm = McpToolError::from_error(Error::Wezterm(
            WeztermError::CommandFailed(secret.clone()),
        ));
        assert_eq!(wezterm.code, MCP_ERR_WEZTERM);
        assert_eq!(wezterm.message, "Backend bridge request failed");
        assert!(!wezterm.message.contains("backend-secret"));

        let workflow = McpToolError::from_error(Error::Workflow(
            crate::error::WorkflowError::Aborted(secret.clone()),
        ));
        assert_eq!(workflow.code, MCP_ERR_WORKFLOW);
        assert_eq!(workflow.message, "Workflow request failed");

        let policy = McpToolError::from_error(Error::Policy(secret));
        assert_eq!(policy.code, MCP_ERR_POLICY);
        assert_eq!(policy.message, "Policy rejected the request");
    }

    #[test]
    fn mcp_tool_error_from_strict_no_db_bridge_error_uses_public_message() {
        let err = Error::runtime_backend(
            "mcp_bridge.build_server_with_db",
            "build_server_with_db called with db_path=None and skipped wa.secret_tool",
        );
        let mcp_err = McpToolError::from_error(err);

        assert_eq!(mcp_err.code, MCP_ERR_NOT_IMPLEMENTED);
        assert_eq!(mcp_err.message, "MCP surface unavailable");
        assert!(!mcp_err.message.contains("db_path=None"));
        assert!(!mcp_err.message.contains("wa.secret_tool"));
        assert!(mcp_err.hint.unwrap().contains("build_server_degraded"));
    }

    // ========================================================================
    // map_mcp_error Tests
    // ========================================================================

    #[test]
    fn map_error_pane_not_found() {
        let err = Error::Wezterm(WeztermError::PaneNotFound(99));
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_PANE_NOT_FOUND);
        assert!(hint.unwrap().contains("wa.state"));
    }

    #[test]
    fn map_error_timeout() {
        let err = Error::Wezterm(WeztermError::Timeout(5000));
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_TIMEOUT);
        assert!(hint.is_some());
    }

    #[test]
    fn map_error_not_running() {
        let err = Error::Wezterm(WeztermError::NotRunning);
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_WEZTERM);
        assert!(hint.unwrap().contains("running"));
    }

    #[test]
    fn map_error_cli_not_found() {
        let err = Error::Wezterm(WeztermError::CliNotFound);
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_WEZTERM);
        assert!(hint.unwrap().contains("PATH"));
    }

    #[test]
    fn indeterminate_effects_have_a_dedicated_no_retry_mcp_contract() {
        for error in [
            Error::Wezterm(WeztermError::IndeterminateMutation {
                operation: "spawn",
            }),
            Error::Storage(StorageError::IndeterminateMutation {
                operation: "store_embedding",
            }),
            Error::Storage(StorageError::WriterSettlementIndeterminate {
                phase: "command_response",
            }),
        ] {
            let (code, hint) = map_mcp_error(&error);
            assert_eq!(code, MCP_ERR_INDETERMINATE_EFFECT);
            let hint = hint.expect("indeterminate MCP hint").to_ascii_lowercase();
            assert!(hint.contains("reconcile"));
            assert!(hint.contains("do not retry automatically"));

            let envelope = McpToolError::from_error(error);
            assert_eq!(envelope.code, MCP_ERR_INDETERMINATE_EFFECT);
            assert_eq!(envelope.message, "Operation outcome is indeterminate");
            assert!(
                envelope
                    .hint
                    .is_some_and(|text| text.contains("do not retry automatically"))
            );
        }
    }

    #[test]
    fn map_error_config() {
        let err = Error::Config(crate::error::ConfigError::FileNotFound(
            "test.toml".to_string(),
        ));
        let (code, _) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_CONFIG);
    }

    #[test]
    fn map_error_storage() {
        let err = Error::Storage(crate::error::StorageError::Database("db error".to_string()));
        let (code, _) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_STORAGE);
    }

    #[test]
    fn map_error_reservation_conflict() {
        let err = Error::Storage(crate::error::StorageError::ReservationConflict {
            pane_id: 12,
            existing_id: 44,
        });
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_RESERVATION_CONFLICT);
        assert!(hint.unwrap().contains("wa.reservations"));
    }

    #[test]
    fn map_error_workflow() {
        let err = Error::Workflow(crate::error::WorkflowError::Aborted(
            "step failed".to_string(),
        ));
        let (code, _) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_WORKFLOW);
    }

    #[test]
    fn map_error_policy() {
        let err = Error::Policy("denied".to_string());
        let (code, _) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_POLICY);
    }

    #[test]
    fn map_error_cancellation_is_a_retryable_timeout_not_storage_or_internal() {
        let err = Error::Cancelled("request budget exhausted".to_string());
        let (code, hint) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_TIMEOUT);
        assert!(
            hint.is_some_and(|hint| !hint.is_empty() && hint.contains("Retry")),
            "cancellation must carry actionable retry guidance"
        );
    }

    #[test]
    fn map_error_runtime_falls_through() {
        let err = Error::runtime_backend("mcp test runtime", "unexpected");
        let (code, _) = map_mcp_error(&err);
        assert_eq!(code, MCP_ERR_INTERNAL);
    }

    #[test]
    fn map_strict_no_db_bridge_error_is_not_implemented() {
        let err = Error::runtime_backend(
            "mcp_bridge.build_server_with_db",
            "build_server_with_db called with db_path=None",
        );
        let (code, hint) = map_mcp_error(&err);

        assert_eq!(code, MCP_ERR_NOT_IMPLEMENTED);
        assert!(hint.unwrap().contains("build_server_degraded"));
    }

    // ========================================================================
    // map_caut_error Tests
    // ========================================================================

    #[test]
    fn map_caut_not_installed() {
        let err = CautError::NotInstalled;
        let (code, hint) = map_caut_error(&err);
        assert_eq!(code, MCP_ERR_CONFIG);
        assert!(hint.unwrap().contains("caut"));
    }

    #[test]
    fn map_caut_timeout() {
        let err = CautError::Timeout { timeout_secs: 5 };
        let (code, hint) = map_caut_error(&err);
        assert_eq!(code, MCP_ERR_TIMEOUT);
        assert!(hint.is_some());
    }

    #[test]
    fn map_caut_other_error_uses_remediation() {
        let err = CautError::Io {
            message: "connection refused".to_string(),
        };
        let (code, hint) = map_caut_error(&err);
        assert_eq!(code, MCP_ERR_CAUT);
        assert!(hint.is_some());
    }

    // ========================================================================
    // map_cass_error Tests
    // ========================================================================

    #[test]
    fn map_cass_not_installed() {
        let err = CassError::NotInstalled;
        let (code, hint) = map_cass_error(&err);
        assert_eq!(code, MCP_ERR_CONFIG);
        assert!(hint.unwrap().contains("cass"));
    }

    #[test]
    fn map_cass_timeout() {
        let err = CassError::Timeout { timeout_secs: 5 };
        let (code, hint) = map_cass_error(&err);
        assert_eq!(code, MCP_ERR_TIMEOUT);
        assert!(hint.is_some());
    }

    #[test]
    fn map_cass_other_error_uses_remediation() {
        let err = CassError::Io {
            message: "pipe broken".to_string(),
        };
        let (code, hint) = map_cass_error(&err);
        assert_eq!(code, MCP_ERR_CASS);
        assert!(hint.is_some());
    }

    #[test]
    fn auxiliary_tool_errors_classify_before_rendering_untrusted_detail() {
        let secret = format!("aux-secret-{}", "z".repeat(128 * 1024));
        let caut = McpToolError::from_caut_error(CautError::Io {
            message: secret.clone(),
        });
        assert_eq!(caut.code, MCP_ERR_CAUT);
        assert_eq!(caut.message, "caut request failed");
        assert!(!caut.message.contains("aux-secret"));
        assert!(caut.hint.is_some_and(|hint| hint.len() < 256));

        let cass = McpToolError::from_cass_error(CassError::NoResults { query: secret });
        assert_eq!(cass.code, MCP_ERR_CASS);
        assert_eq!(cass.message, "cass request failed");
        assert!(!cass.message.contains("aux-secret"));
        assert!(cass.hint.is_some_and(|hint| hint.len() < 256));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn prop_mcp_tool_error_from_caut_timeout_aligns_with_mapper(timeout_secs in any::<u64>()) {
            let err = CautError::Timeout { timeout_secs };
            let (code, hint) = map_caut_error(&err);
            let tool_err = McpToolError::from_caut_error(err);

            prop_assert_eq!(tool_err.code, code);
            prop_assert_eq!(tool_err.message, "caut request timed out");
            prop_assert_eq!(tool_err.hint, hint);
            prop_assert_eq!(tool_err.code, MCP_ERR_TIMEOUT);
        }

        #[test]
        fn prop_mcp_tool_error_from_cass_timeout_aligns_with_mapper(timeout_secs in any::<u64>()) {
            let err = CassError::Timeout { timeout_secs };
            let (code, hint) = map_cass_error(&err);
            let tool_err = McpToolError::from_cass_error(err);

            prop_assert_eq!(tool_err.code, code);
            prop_assert_eq!(tool_err.message, "cass request timed out");
            prop_assert_eq!(tool_err.hint, hint);
            prop_assert_eq!(tool_err.code, MCP_ERR_TIMEOUT);
        }

        #[test]
        fn prop_mcp_tool_error_from_error_aligns_with_mapper_for_pane_not_found(pane_id in any::<u64>()) {
            let err = Error::Wezterm(WeztermError::PaneNotFound(pane_id));
            let (code, hint) = map_mcp_error(&err);
            let tool_err = McpToolError::from_error(err);

            prop_assert_eq!(tool_err.code, code);
            prop_assert_eq!(tool_err.message, format!("Pane not found: {pane_id}"));
            prop_assert_eq!(tool_err.hint, hint);
            prop_assert_eq!(tool_err.code, MCP_ERR_PANE_NOT_FOUND);
        }
    }
}
