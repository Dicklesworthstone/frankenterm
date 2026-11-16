use crate::{PaneId, PaneRegistrationHandle};
use chrono::serde::ts_milliseconds;
use chrono::{DateTime, Utc};
use serde::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

static CLIENT_ID: AtomicUsize = AtomicUsize::new(0);
const CLIENT_REGISTRATION_RETIRED: usize = 1usize << (usize::BITS - 1);
const CLIENT_REGISTRATION_OPERATION_MASK: usize = CLIENT_REGISTRATION_RETIRED - 1;
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
        let id = crate::next_unique_usize_id(&CLIENT_ID, "mux client");
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

#[derive(Deserialize, Serialize, Debug, Clone)]
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
    /// Exact process-local authority for the focused pane registration.
    ///
    /// This is deliberately omitted from the wire representation. A decoded
    /// numeric pane ID is metadata, not authority to synthesize callbacks
    /// against whichever pane later occupies that reusable slot.
    #[serde(skip, default)]
    focused_pane_registration: Option<PaneRegistrationHandle>,
    /// Exact process-local lifetime authority for this client registration.
    ///
    /// It is deliberately absent from the wire representation. Equal-valued
    /// `ClientId`s may be registered by successive connections, but deferred
    /// work must retain the generation that admitted it rather than rechecking
    /// whichever allocation currently occupies the value-keyed map slot.
    #[serde(skip, default)]
    pub(crate) registration_generation: Arc<ClientRegistrationGeneration>,
}

impl ClientInfo {
    pub fn new(client_id: Arc<ClientId>) -> Self {
        Self {
            client_id,
            connected_at: Utc::now(),
            active_workspace: None,
            last_input: Utc::now(),
            focused_pane_id: None,
            focused_pane_registration: None,
            registration_generation: Arc::new(ClientRegistrationGeneration::default()),
        }
    }

    pub fn from_wire_parts(
        client_id: Arc<ClientId>,
        connected_at: DateTime<Utc>,
        active_workspace: Option<String>,
        last_input: DateTime<Utc>,
        focused_pane_id: Option<PaneId>,
    ) -> Self {
        Self {
            client_id,
            connected_at,
            active_workspace,
            last_input,
            focused_pane_id,
            focused_pane_registration: None,
            registration_generation: Arc::new(ClientRegistrationGeneration::default()),
        }
    }

    pub fn update_last_input(&mut self) {
        self.last_input = Utc::now();
    }

    pub(crate) fn replace_focused_pane(
        &mut self,
        pane_id: PaneId,
        registration: Option<PaneRegistrationHandle>,
    ) -> Option<PaneRegistrationHandle> {
        self.focused_pane_id.replace(pane_id);
        std::mem::replace(&mut self.focused_pane_registration, registration)
    }

    pub(crate) fn clear_focused_pane(&mut self) {
        self.focused_pane_id = None;
        self.focused_pane_registration = None;
    }

    pub(crate) fn focused_pane_registration(&self) -> Option<PaneRegistrationHandle> {
        self.focused_pane_registration.clone()
    }

    pub(crate) fn wire_snapshot(&self) -> Self {
        let mut snapshot = self.clone();
        snapshot.client_id = Arc::new(self.client_id.as_ref().clone());
        snapshot.focused_pane_registration = None;
        snapshot.registration_generation = Arc::new(ClientRegistrationGeneration::default());
        snapshot
    }
}

/// Admission state for one exact process-local client registration.
///
/// Retirement sets the high bit and therefore closes acquisition in the same
/// `clients` write cut that removes or replaces the registration. Operations
/// admitted before that cut retain a counted lease and may finish only with
/// the exact `ClientOperationGuard` that owns it.
#[derive(Debug, Default)]
pub(crate) struct ClientRegistrationGeneration {
    operation_state: AtomicUsize,
}

impl ClientRegistrationGeneration {
    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<ClientRegistrationOperationLease> {
        let mut state = self.operation_state.load(Ordering::Acquire);
        loop {
            if state & CLIENT_REGISTRATION_RETIRED != 0 {
                return None;
            }
            let active = state & CLIENT_REGISTRATION_OPERATION_MASK;
            let next = active.checked_add(1)?;
            if next > CLIENT_REGISTRATION_OPERATION_MASK {
                return None;
            }
            match self.operation_state.compare_exchange_weak(
                state,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ClientRegistrationOperationLease {
                        generation: Arc::clone(self),
                    });
                }
                Err(actual) => state = actual,
            }
        }
    }

    pub(crate) fn retire(&self) {
        self.operation_state
            .fetch_or(CLIENT_REGISTRATION_RETIRED, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn is_retired(&self) -> bool {
        self.operation_state.load(Ordering::Acquire) & CLIENT_REGISTRATION_RETIRED != 0
    }

    #[cfg(test)]
    pub(crate) fn active_operations(&self) -> usize {
        self.operation_state.load(Ordering::Acquire) & CLIENT_REGISTRATION_OPERATION_MASK
    }
}

/// Counted, non-cloneable admission lease for one client generation.
#[derive(Debug)]
pub(crate) struct ClientRegistrationOperationLease {
    generation: Arc<ClientRegistrationGeneration>,
}

impl Drop for ClientRegistrationOperationLease {
    fn drop(&mut self) {
        let previous = self
            .generation
            .operation_state
            .fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous & CLIENT_REGISTRATION_OPERATION_MASK > 0,
            "client registration operation count must not underflow"
        );
    }
}

/// Equality follows the serialized five-field client projection.
///
/// The wire schema records timestamps as integer milliseconds and omits
/// process-local pane authority. Comparing with greater timestamp precision
/// would make a value unequal to its own successful wire roundtrip.
impl PartialEq for ClientInfo {
    fn eq(&self, other: &Self) -> bool {
        self.client_id == other.client_id
            && self.connected_at.timestamp_millis() == other.connected_at.timestamp_millis()
            && self.active_workspace == other.active_workspace
            && self.last_input.timestamp_millis() == other.last_input.timestamp_millis()
            && self.focused_pane_id == other.focused_pane_id
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
        info.replace_focused_pane(42, None);
        assert_eq!(info.focused_pane_id, Some(42));
        info.replace_focused_pane(99, None);
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
        let info = ClientInfo::from_wire_parts(cid, ts, None, ts, None);

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
    fn client_info_equality_uses_wire_timestamp_precision() {
        use chrono::TimeZone;

        let client_id = Arc::new(make_client_id("wire-equality", 2));
        let first = Utc
            .timestamp_opt(1_700_000_000, 123_456_789)
            .single()
            .expect("valid first timestamp");
        let same_wire_millisecond = Utc
            .timestamp_opt(1_700_000_000, 123_999_999)
            .single()
            .expect("valid same-millisecond timestamp");
        let next_wire_millisecond = Utc
            .timestamp_opt(1_700_000_000, 124_000_000)
            .single()
            .expect("valid next-millisecond timestamp");

        let original =
            ClientInfo::from_wire_parts(Arc::clone(&client_id), first, None, first, None);
        let same_projection = ClientInfo::from_wire_parts(
            Arc::clone(&client_id),
            same_wire_millisecond,
            None,
            same_wire_millisecond,
            None,
        );
        let next_projection = ClientInfo::from_wire_parts(
            client_id,
            next_wire_millisecond,
            None,
            next_wire_millisecond,
            None,
        );

        assert_eq!(
            original, same_projection,
            "sub-millisecond differences omitted by the wire schema must compare equal",
        );
        assert_ne!(
            original, next_projection,
            "adjacent serialized milliseconds must remain distinguishable",
        );

        let json = serde_json::to_string(&original).expect("serialize sub-millisecond client");
        let decoded: ClientInfo =
            serde_json::from_str(&json).expect("deserialize sub-millisecond client");
        assert_eq!(
            decoded, original,
            "wire equality must be stable across sub-millisecond truncation",
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
