use crate::PaneId;
use chrono::serde::ts_milliseconds;
use chrono::{DateTime, Utc};
use serde::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

static CLIENT_ID: AtomicUsize = AtomicUsize::new(0);
lazy_static::lazy_static! {
    static ref EPOCH: u64 = SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap().as_secs();
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClientId {
    pub hostname: String,
    pub username: String,
    pub pid: u32,
    pub epoch: u64,
    pub id: usize,
    pub ssh_auth_sock: Option<String>,
}

impl ClientId {
    pub fn new() -> Self {
        let id = CLIENT_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            hostname: hostname::get()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|_| "localhost".to_string()),
            username: config::username_from_env().unwrap_or_else(|_| "somebody".to_string()),
            pid: unsafe { libc::getpid() as u32 },
            epoch: *EPOCH,
            id,
            ssh_auth_sock: crate::AgentProxy::default_ssh_auth_sock(),
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ClientInfo {
    pub client_id: Arc<ClientId>,
    /// The time this client last connected
    ///
    /// [ft-ztcsl] Serialized as milliseconds-since-epoch rather than
    /// seconds so a JSON roundtrip preserves the sub-second portion of
    /// the timestamp. `ts_seconds` truncated `Utc::now()`'s nanosecond
    /// precision to whole seconds, so any `ClientInfo` that went
    /// through the wire once came back coarsened — two clients that
    /// connected within the same second became JSON-equal.
    #[serde(with = "ts_milliseconds")]
    pub connected_at: DateTime<Utc>,
    /// Which workspace is active
    pub active_workspace: Option<String>,
    /// The last time we received input from this client
    ///
    /// [ft-ztcsl] Same millisecond-resolution wire format as
    /// `connected_at` for the same roundtrip-precision reason.
    #[serde(with = "ts_milliseconds")]
    pub last_input: DateTime<Utc>,
    /// The currently-focused pane
    pub focused_pane_id: Option<PaneId>,
}

impl ClientInfo {
    pub fn new(client_id: Arc<ClientId>) -> Self {
        Self {
            client_id,
            connected_at: Utc::now(),
            active_workspace: None,
            last_input: Utc::now(),
            focused_pane_id: None,
        }
    }

    pub fn update_last_input(&mut self) {
        self.last_input = Utc::now();
    }

    pub fn update_focused_pane(&mut self, pane_id: PaneId) {
        self.focused_pane_id.replace(pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_client_id(hostname: &str, pid: u32) -> ClientId {
        ClientId {
            hostname: hostname.to_string(),
            username: "testuser".to_string(),
            pid,
            epoch: 1000,
            id: 0,
            ssh_auth_sock: None,
        }
    }

    #[test]
    fn client_id_equality() {
        let a = make_client_id("host1", 100);
        let b = make_client_id("host1", 100);
        assert_eq!(a, b);
    }

    #[test]
    fn client_id_inequality_hostname() {
        let a = make_client_id("host1", 100);
        let b = make_client_id("host2", 100);
        assert_ne!(a, b);
    }

    #[test]
    fn client_id_inequality_pid() {
        let a = make_client_id("host1", 100);
        let b = make_client_id("host1", 200);
        assert_ne!(a, b);
    }

    #[test]
    fn client_id_clone() {
        let a = make_client_id("host1", 100);
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn client_id_debug() {
        let id = make_client_id("myhost", 42);
        let dbg = format!("{:?}", id);
        assert!(dbg.contains("ClientId"));
        assert!(dbg.contains("myhost"));
        assert!(dbg.contains("42"));
    }

    #[test]
    fn client_id_hash() {
        let a = make_client_id("host1", 100);
        let b = make_client_id("host1", 100);
        let c = make_client_id("host2", 200);
        let mut set = HashSet::new();
        set.insert(a);
        set.insert(b); // duplicate
        set.insert(c);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn client_id_with_ssh_auth_sock() {
        let id = ClientId {
            ssh_auth_sock: Some("/tmp/ssh-agent.sock".to_string()),
            ..make_client_id("host", 1)
        };
        assert_eq!(id.ssh_auth_sock, Some("/tmp/ssh-agent.sock".to_string()));
    }

    #[test]
    fn client_info_new() {
        let cid = Arc::new(make_client_id("host", 1));
        let info = ClientInfo::new(cid.clone());
        assert_eq!(info.client_id, cid);
        assert!(info.active_workspace.is_none());
        assert!(info.focused_pane_id.is_none());
    }

    #[test]
    fn client_info_update_last_input() {
        let cid = Arc::new(make_client_id("host", 1));
        let mut info = ClientInfo::new(cid);
        let before = info.last_input;
        // May or may not change depending on timing, but should not panic
        info.update_last_input();
        assert!(info.last_input >= before);
    }

    #[test]
    fn client_info_update_focused_pane() {
        let cid = Arc::new(make_client_id("host", 1));
        let mut info = ClientInfo::new(cid);
        assert!(info.focused_pane_id.is_none());
        info.update_focused_pane(42);
        assert_eq!(info.focused_pane_id, Some(42));
        info.update_focused_pane(99);
        assert_eq!(info.focused_pane_id, Some(99));
    }

    // [ft-ztcsl] Pre-fix, ClientInfo::{connected_at, last_input}
    // serialized via chrono::serde::ts_seconds — a JSON roundtrip
    // truncated the subsecond portion of the timestamp to whole
    // seconds, so two clients that connected inside the same second
    // became JSON-equal. The switch to ts_milliseconds preserves
    // millisecond resolution, enough to distinguish connections that
    // happen in the same wall-clock second.
    #[test]
    fn client_info_json_roundtrip_preserves_subsecond_ft_ztcsl() {
        use chrono::TimeZone;

        let cid = Arc::new(make_client_id("host", 1));
        // Construct a timestamp with a non-zero millisecond remainder
        // so the pre-fix ts_seconds path would visibly lose precision.
        let ts = Utc
            .timestamp_opt(1_700_000_000, 123_000_000)
            .single()
            .expect("valid chrono timestamp");
        let info = ClientInfo {
            client_id: cid,
            connected_at: ts,
            active_workspace: None,
            last_input: ts,
            focused_pane_id: None,
        };

        let json = serde_json::to_string(&info).expect("serialize ClientInfo");
        let back: ClientInfo =
            serde_json::from_str(&json).expect("deserialize ClientInfo roundtrip");

        // Millisecond precision must survive. The pre-fix ts_seconds
        // encoding would truncate .123 seconds to .000, making this
        // assertion fail.
        assert_eq!(
            back.connected_at, info.connected_at,
            "ft-ztcsl: connected_at must roundtrip with millisecond precision"
        );
        assert_eq!(
            back.last_input, info.last_input,
            "ft-ztcsl: last_input must roundtrip with millisecond precision"
        );
        // Explicit subsecond sanity check — prevents a future regress
        // that flips back to ts_seconds from passing the whole-second
        // equality comparison above by coincidence.
        assert_eq!(
            back.connected_at.timestamp_subsec_millis(),
            123,
            "ft-ztcsl: the 123ms remainder must survive the roundtrip"
        );
    }

    #[test]
    fn client_info_clone() {
        let cid = Arc::new(make_client_id("host", 1));
        let info = ClientInfo::new(cid);
        let cloned = info.clone();
        assert_eq!(info, cloned);
    }

    #[test]
    fn client_info_debug() {
        let cid = Arc::new(make_client_id("host", 1));
        let info = ClientInfo::new(cid);
        let dbg = format!("{:?}", info);
        assert!(dbg.contains("ClientInfo"));
        assert!(dbg.contains("host"));
    }

    #[test]
    fn client_info_with_workspace() {
        let cid = Arc::new(make_client_id("host", 1));
        let mut info = ClientInfo::new(cid);
        info.active_workspace = Some("default".to_string());
        assert_eq!(info.active_workspace, Some("default".to_string()));
    }
}
