//! MCP error code definitions and error mapping utilities.

use crate::cass::CassError;
use crate::caut::CautError;
use crate::error::{Error, WeztermError};

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
            message: err.to_string(),
            hint,
        }
    }

    pub(crate) fn from_caut_error(err: CautError) -> Self {
        let (code, hint) = map_caut_error(&err);
        Self {
            code,
            message: err.to_string(),
            hint,
        }
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
        _ => (MCP_ERR_CAUT, Some(error.remediation().summary.to_string())),
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
        _ => (MCP_ERR_CASS, Some(error.remediation().summary.to_string())),
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
        Error::Wezterm(_) => (MCP_ERR_WEZTERM, None),
        Error::Config(_) => (MCP_ERR_CONFIG, None),
        Error::Storage(_) => (MCP_ERR_STORAGE, None),
        Error::Workflow(_) => (MCP_ERR_WORKFLOW, None),
        Error::Policy(_) => (MCP_ERR_POLICY, None),
        _ => (MCP_ERR_NOT_IMPLEMENTED, None),
    }
}
