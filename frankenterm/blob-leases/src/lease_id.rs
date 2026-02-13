use uuid::Uuid;

/// Represents an individual lease
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub struct LeaseId {
    uuid: Uuid,
    pid: u32,
}

impl std::fmt::Display for LeaseId {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "lease:pid={},{}", self.pid, self.uuid.hyphenated())
    }
}

impl LeaseId {
    pub fn new() -> Self {
        let uuid = Uuid::new_v4();
        let pid = std::process::id();
        Self { uuid, pid }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_captures_current_pid() {
        let id = LeaseId::new();
        assert_eq!(id.pid(), std::process::id());
    }

    #[test]
    fn two_lease_ids_are_unique() {
        let a = LeaseId::new();
        let b = LeaseId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn display_contains_pid() {
        let id = LeaseId::new();
        let display = format!("{id}");
        let expected_pid = format!("lease:pid={},", std::process::id());
        assert!(display.starts_with(&expected_pid), "got: {display}");
    }

    #[test]
    fn display_contains_uuid_with_hyphens() {
        let id = LeaseId::new();
        let display = format!("{id}");
        // UUID hyphenated format has exactly 4 hyphens in the UUID portion
        let uuid_part = display.split(',').nth(1).unwrap();
        assert_eq!(uuid_part.matches('-').count(), 4);
    }

    #[test]
    fn clone_produces_equal_id() {
        let id = LeaseId::new();
        let cloned = id;
        assert_eq!(id, cloned);
    }

    #[test]
    fn debug_output_is_meaningful() {
        let id = LeaseId::new();
        let debug = format!("{id:?}");
        assert!(debug.contains("LeaseId"));
        assert!(debug.contains("uuid"));
        assert!(debug.contains("pid"));
    }
}
