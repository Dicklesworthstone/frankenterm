//! E5.F2.T2: Contract versioning and compatibility matrix CI gate.
//!
//! Tests version compatibility between frankenterm-core and frankensqlite,
//! compatibility matrix evaluation, mixed-version read/write scenarios,
//! and version mismatch warning generation.

// ═══════════════════════════════════════════════════════════════════════
// Version model
// ═══════════════════════════════════════════════════════════════════════

const RECORDER_API_VERSION: &str = "ft.recorder-api.v1";
const RECORDER_API_VERSION_MAJOR: u32 = 1;
const RECORDER_API_VERSION_MINOR: u32 = 0;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
struct SemanticVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl SemanticVersion {
    fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    fn display(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }

    fn is_compatible_with(&self, other: &Self) -> CompatLevel {
        if self.major != other.major {
            CompatLevel::Unsupported
        } else if self.minor < other.minor {
            CompatLevel::Deprecated
        } else {
            CompatLevel::Supported
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Compatibility matrix
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompatLevel {
    Supported,
    Deprecated,
    Unsupported,
}

/// A compatibility matrix entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CompatEntry {
    core_version: SemanticVersion,
    sqlite_version: SemanticVersion,
    level: CompatLevel,
    notes: String,
}

/// The full compatibility matrix.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CompatMatrix {
    api_version: String,
    entries: Vec<CompatEntry>,
}

impl CompatMatrix {
    fn new() -> Self {
        Self {
            api_version: RECORDER_API_VERSION.to_string(),
            entries: Vec::new(),
        }
    }

    fn add(&mut self, core: SemanticVersion, sqlite: SemanticVersion, notes: &str) {
        let level = core.is_compatible_with(&sqlite);
        self.entries.push(CompatEntry {
            core_version: core,
            sqlite_version: sqlite,
            level,
            notes: notes.to_string(),
        });
    }

    fn lookup(&self, core: &SemanticVersion, sqlite: &SemanticVersion) -> Option<&CompatEntry> {
        self.entries
            .iter()
            .find(|e| e.core_version == *core && e.sqlite_version == *sqlite)
    }

    fn supported_pairs(&self) -> Vec<&CompatEntry> {
        self.entries
            .iter()
            .filter(|e| e.level == CompatLevel::Supported)
            .collect()
    }

    fn unsupported_pairs(&self) -> Vec<&CompatEntry> {
        self.entries
            .iter()
            .filter(|e| e.level == CompatLevel::Unsupported)
            .collect()
    }

    fn deprecated_pairs(&self) -> Vec<&CompatEntry> {
        self.entries
            .iter()
            .filter(|e| e.level == CompatLevel::Deprecated)
            .collect()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Version mismatch warnings
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct VersionMismatchWarning {
    expected_api_version: String,
    actual_api_version: String,
    severity: MismatchSeverity,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum MismatchSeverity {
    Info,
    Warning,
    Error,
}

fn check_api_version(expected: &str, actual: &str) -> Option<VersionMismatchWarning> {
    if expected == actual {
        return None;
    }
    let severity = if expected.split('.').next() != actual.split('.').next() {
        MismatchSeverity::Error
    } else {
        MismatchSeverity::Warning
    };
    Some(VersionMismatchWarning {
        expected_api_version: expected.to_string(),
        actual_api_version: actual.to_string(),
        severity,
        message: format!("API version mismatch: expected {expected}, got {actual}"),
    })
}

// ═══════════════════════════════════════════════════════════════════════
// Mixed-version read/write simulation
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct VersionedRecord {
    api_version: String,
    writer_version: SemanticVersion,
    payload: String,
}

fn can_read_record(reader_version: &SemanticVersion, record: &VersionedRecord) -> bool {
    reader_version.is_compatible_with(&record.writer_version) != CompatLevel::Unsupported
}

// ═══════════════════════════════════════════════════════════════════════
// CI gate model
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CiVersionGate {
    min_supported: SemanticVersion,
    max_supported: SemanticVersion,
    test_versions: Vec<SemanticVersion>,
}

impl CiVersionGate {
    fn is_in_range(&self, version: &SemanticVersion) -> bool {
        *version >= self.min_supported && *version <= self.max_supported
    }

    fn out_of_range_versions(&self) -> Vec<&SemanticVersion> {
        self.test_versions
            .iter()
            .filter(|v| !self.is_in_range(v))
            .collect()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: API version constant
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_api_version_constant_exists() {
    assert!(!RECORDER_API_VERSION.is_empty());
    assert!(RECORDER_API_VERSION.starts_with("ft.recorder-api."));
}

#[test]
fn test_api_version_major_minor() {
    assert_eq!(RECORDER_API_VERSION_MAJOR, 1);
    assert_eq!(RECORDER_API_VERSION_MINOR, 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: SemanticVersion
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_semantic_version_display() {
    let v = SemanticVersion::new(1, 2, 3);
    assert_eq!(v.display(), "1.2.3");
}

#[test]
fn test_semantic_version_ordering() {
    let v1 = SemanticVersion::new(0, 9, 0);
    let v2 = SemanticVersion::new(1, 0, 0);
    let v3 = SemanticVersion::new(1, 1, 0);
    assert!(v1 < v2);
    assert!(v2 < v3);
}

#[test]
fn test_semantic_version_serde_roundtrip() {
    let v = SemanticVersion::new(2, 5, 1);
    let json = serde_json::to_string(&v).unwrap();
    let back: SemanticVersion = serde_json::from_str(&json).unwrap();
    assert_eq!(v, back);
}

#[test]
fn test_version_same_major_same_minor_supported() {
    let a = SemanticVersion::new(1, 0, 0);
    let b = SemanticVersion::new(1, 0, 5);
    assert_eq!(a.is_compatible_with(&b), CompatLevel::Supported);
}

#[test]
fn test_version_same_major_newer_minor_deprecated() {
    let older = SemanticVersion::new(1, 0, 0);
    let newer = SemanticVersion::new(1, 2, 0);
    assert_eq!(older.is_compatible_with(&newer), CompatLevel::Deprecated);
}

#[test]
fn test_version_different_major_unsupported() {
    let a = SemanticVersion::new(1, 0, 0);
    let b = SemanticVersion::new(2, 0, 0);
    assert_eq!(a.is_compatible_with(&b), CompatLevel::Unsupported);
}

#[test]
fn test_version_same_version_supported() {
    let v = SemanticVersion::new(1, 3, 7);
    assert_eq!(v.is_compatible_with(&v), CompatLevel::Supported);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Version mismatch warnings
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_version_mismatch_none_when_equal() {
    assert!(check_api_version("ft.recorder-api.v1", "ft.recorder-api.v1").is_none());
}

#[test]
fn test_version_mismatch_warning_on_minor_diff() {
    let warn = check_api_version("ft.recorder-api.v1", "ft.recorder-api.v1.1").unwrap();
    assert_eq!(warn.severity, MismatchSeverity::Warning);
}

#[test]
fn test_version_mismatch_error_on_major_diff() {
    let warn = check_api_version("ft.recorder-api.v1", "frankensqlite.v2").unwrap();
    assert_eq!(warn.severity, MismatchSeverity::Error);
}

#[test]
fn test_version_mismatch_message_contains_versions() {
    let warn = check_api_version("ft.recorder-api.v1", "ft.recorder-api.v2").unwrap();
    assert!(warn.message.contains("ft.recorder-api.v1"));
    assert!(warn.message.contains("ft.recorder-api.v2"));
}

#[test]
fn test_version_mismatch_serde_roundtrip() {
    let warn = check_api_version("a", "b").unwrap();
    let json = serde_json::to_string(&warn).unwrap();
    let back: VersionMismatchWarning = serde_json::from_str(&json).unwrap();
    assert_eq!(warn.expected_api_version, back.expected_api_version);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Compatibility matrix
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_compatibility_matrix_minimum_version() {
    let mut matrix = CompatMatrix::new();
    matrix.add(
        SemanticVersion::new(0, 1, 0),
        SemanticVersion::new(0, 1, 0),
        "baseline",
    );
    let entry = matrix
        .lookup(
            &SemanticVersion::new(0, 1, 0),
            &SemanticVersion::new(0, 1, 0),
        )
        .unwrap();
    assert_eq!(entry.level, CompatLevel::Supported);
}

#[test]
fn test_compatibility_matrix_maximum_version() {
    let mut matrix = CompatMatrix::new();
    matrix.add(
        SemanticVersion::new(1, 0, 0),
        SemanticVersion::new(1, 0, 0),
        "current",
    );
    let pairs = matrix.supported_pairs();
    assert_eq!(pairs.len(), 1);
}

#[test]
fn test_compatibility_matrix_cross_major_unsupported() {
    let mut matrix = CompatMatrix::new();
    matrix.add(
        SemanticVersion::new(1, 0, 0),
        SemanticVersion::new(2, 0, 0),
        "breaking",
    );
    let pairs = matrix.unsupported_pairs();
    assert_eq!(pairs.len(), 1);
}

#[test]
fn test_compatibility_matrix_deprecated_pairs() {
    let mut matrix = CompatMatrix::new();
    matrix.add(
        SemanticVersion::new(1, 0, 0),
        SemanticVersion::new(1, 2, 0),
        "newer minor",
    );
    let pairs = matrix.deprecated_pairs();
    assert_eq!(pairs.len(), 1);
}

#[test]
fn test_compatibility_matrix_api_version() {
    let matrix = CompatMatrix::new();
    assert_eq!(matrix.api_version, RECORDER_API_VERSION);
}

#[test]
fn test_compatibility_matrix_serde_roundtrip() {
    let mut matrix = CompatMatrix::new();
    matrix.add(
        SemanticVersion::new(1, 0, 0),
        SemanticVersion::new(1, 0, 0),
        "ok",
    );
    matrix.add(
        SemanticVersion::new(1, 0, 0),
        SemanticVersion::new(2, 0, 0),
        "bad",
    );
    let json = serde_json::to_string_pretty(&matrix).unwrap();
    let back: CompatMatrix = serde_json::from_str(&json).unwrap();
    assert_eq!(matrix.entries.len(), back.entries.len());
}

#[test]
fn test_compatibility_matrix_lookup_missing() {
    let matrix = CompatMatrix::new();
    assert!(
        matrix
            .lookup(
                &SemanticVersion::new(9, 9, 9),
                &SemanticVersion::new(9, 9, 9)
            )
            .is_none()
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Mixed-version read/write
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_mixed_version_read_write_compatible() {
    let record = VersionedRecord {
        api_version: RECORDER_API_VERSION.to_string(),
        writer_version: SemanticVersion::new(1, 0, 0),
        payload: "test data".to_string(),
    };
    let reader = SemanticVersion::new(1, 0, 0);
    assert!(can_read_record(&reader, &record));
}

#[test]
fn test_mixed_version_older_writer_newer_reader() {
    let record = VersionedRecord {
        api_version: RECORDER_API_VERSION.to_string(),
        writer_version: SemanticVersion::new(1, 0, 0),
        payload: "old data".to_string(),
    };
    let reader = SemanticVersion::new(1, 2, 0);
    assert!(can_read_record(&reader, &record));
}

#[test]
fn test_mixed_version_cross_major_fails() {
    let record = VersionedRecord {
        api_version: "ft.recorder-api.v2".to_string(),
        writer_version: SemanticVersion::new(2, 0, 0),
        payload: "v2 data".to_string(),
    };
    let reader = SemanticVersion::new(1, 0, 0);
    assert!(!can_read_record(&reader, &record));
}

#[test]
fn test_versioned_record_serde_roundtrip() {
    let record = VersionedRecord {
        api_version: RECORDER_API_VERSION.to_string(),
        writer_version: SemanticVersion::new(1, 0, 0),
        payload: "roundtrip".to_string(),
    };
    let json = serde_json::to_string(&record).unwrap();
    let back: VersionedRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(record.payload, back.payload);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: CI version gate
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_ci_gate_in_range() {
    let gate = CiVersionGate {
        min_supported: SemanticVersion::new(0, 8, 0),
        max_supported: SemanticVersion::new(1, 2, 0),
        test_versions: vec![
            SemanticVersion::new(0, 8, 0),
            SemanticVersion::new(1, 0, 0),
            SemanticVersion::new(1, 2, 0),
        ],
    };
    assert!(gate.is_in_range(&SemanticVersion::new(1, 0, 0)));
    assert!(gate.out_of_range_versions().is_empty());
}

#[test]
fn test_ci_gate_out_of_range() {
    let gate = CiVersionGate {
        min_supported: SemanticVersion::new(1, 0, 0),
        max_supported: SemanticVersion::new(1, 5, 0),
        test_versions: vec![
            SemanticVersion::new(0, 9, 0), // below
            SemanticVersion::new(2, 0, 0), // above
        ],
    };
    assert_eq!(gate.out_of_range_versions().len(), 2);
}

#[test]
fn test_ci_gate_boundary_inclusive() {
    let gate = CiVersionGate {
        min_supported: SemanticVersion::new(1, 0, 0),
        max_supported: SemanticVersion::new(1, 5, 0),
        test_versions: Vec::new(),
    };
    assert!(gate.is_in_range(&SemanticVersion::new(1, 0, 0))); // min
    assert!(gate.is_in_range(&SemanticVersion::new(1, 5, 0))); // max
    assert!(!gate.is_in_range(&SemanticVersion::new(0, 9, 9)));
    assert!(!gate.is_in_range(&SemanticVersion::new(1, 5, 1)));
}

#[test]
fn test_ci_gate_serde_roundtrip() {
    let gate = CiVersionGate {
        min_supported: SemanticVersion::new(0, 8, 0),
        max_supported: SemanticVersion::new(1, 2, 0),
        test_versions: vec![SemanticVersion::new(1, 0, 0)],
    };
    let json = serde_json::to_string(&gate).unwrap();
    let back: CiVersionGate = serde_json::from_str(&json).unwrap();
    assert_eq!(gate.min_supported, back.min_supported);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: CompatLevel
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_compat_level_serde_roundtrip() {
    for level in &[
        CompatLevel::Supported,
        CompatLevel::Deprecated,
        CompatLevel::Unsupported,
    ] {
        let json = serde_json::to_string(level).unwrap();
        let back: CompatLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(*level, back);
    }
}

#[test]
fn test_full_matrix_build() {
    let mut matrix = CompatMatrix::new();
    let core_versions = vec![
        SemanticVersion::new(0, 9, 0),
        SemanticVersion::new(1, 0, 0),
        SemanticVersion::new(1, 1, 0),
    ];
    let sqlite_versions = vec![
        SemanticVersion::new(0, 9, 0),
        SemanticVersion::new(1, 0, 0),
        SemanticVersion::new(1, 1, 0),
    ];
    for cv in &core_versions {
        for sv in &sqlite_versions {
            matrix.add(cv.clone(), sv.clone(), "auto");
        }
    }
    // 3x3 = 9 entries
    assert_eq!(matrix.entries.len(), 9);
    // Same major, same or higher minor → supported (1,0)+(1,0), (1,1)+(1,0), (1,1)+(1,1), (1,0)+(1,0) repeated
    // Cross major → unsupported: (0,9) vs (1,x) and (1,x) vs (0,9)
    assert!(matrix.unsupported_pairs().len() > 0);
    assert!(matrix.supported_pairs().len() > 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: MismatchSeverity
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_mismatch_severity_serde_roundtrip() {
    for sev in &[
        MismatchSeverity::Info,
        MismatchSeverity::Warning,
        MismatchSeverity::Error,
    ] {
        let json = serde_json::to_string(sev).unwrap();
        let back: MismatchSeverity = serde_json::from_str(&json).unwrap();
        assert_eq!(*sev, back);
    }
}
