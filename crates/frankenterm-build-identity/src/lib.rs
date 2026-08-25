#![forbid(unsafe_code)]
//! Shared build-time and runtime authority for FrankenTerm component markers.
//!
//! Release tooling supplies one lowercase SHA-256 identity through
//! `FT_ATOMIC_BUILD_IDENTITY`. Ordinary development builds retain the historic
//! `unsealed` marker, but that value is represented by a distinct enum variant
//! and can never be decoded into [`SealedAtomicBuildIdentity`].

use std::error::Error;
use std::fmt;

/// Stable byte prefix consumed by the offline atomic-component verifier.
pub const ATOMIC_COMPONENT_MARKER_PREFIX: &str = "FT_ATOMIC_COMPONENT_IDENTITY_V1:";

/// Historic marker value used only when a development build has no sealed ID.
pub const UNSEALED_BUILD_ID: &str = "unsealed";

/// Closed set of process roles carried by the atomic component marker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AtomicComponentRole {
    /// Standalone `ft` CLI.
    Ft,
    /// Native GUI process.
    FrankenTermGui,
    /// Headless mux-server process.
    FrankenTermMuxServer,
    /// Standalone PTY lifetime guardian.
    FrankenTermPtyGuardian,
}

impl AtomicComponentRole {
    /// Exact stable token embedded in the binary marker.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ft => "ft",
            Self::FrankenTermGui => "frankenterm-gui",
            Self::FrankenTermMuxServer => "frankenterm-mux-server",
            Self::FrankenTermPtyGuardian => "frankenterm-pty-guardian",
        }
    }

    fn from_str(value: &str) -> Result<Self, AtomicComponentIdentityError> {
        match value {
            "ft" => Ok(Self::Ft),
            "frankenterm-gui" => Ok(Self::FrankenTermGui),
            "frankenterm-mux-server" => Ok(Self::FrankenTermMuxServer),
            "frankenterm-pty-guardian" => Ok(Self::FrankenTermPtyGuardian),
            _ => Err(AtomicComponentIdentityError::InvalidComponentRole),
        }
    }
}

impl fmt::Display for AtomicComponentRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Validated and decoded authority for one sealed process-family build.
///
/// Construction accepts exactly 64 lowercase hexadecimal characters. There is
/// deliberately no constructor from a version, inode, path, or arbitrary byte
/// array: those values are not process-family identity authorities.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct SealedAtomicBuildIdentity([u8; 32]);

impl SealedAtomicBuildIdentity {
    /// Decode one canonical lowercase SHA-256 identity.
    pub fn from_lower_hex(value: &str) -> Result<Self, AtomicComponentIdentityError> {
        if value.len() != 64 {
            return Err(AtomicComponentIdentityError::InvalidBuildIdentity);
        }

        let mut decoded = [0_u8; 32];
        for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = decode_lower_hex_nibble(pair[0])
                .ok_or(AtomicComponentIdentityError::InvalidBuildIdentity)?;
            let low = decode_lower_hex_nibble(pair[1])
                .ok_or(AtomicComponentIdentityError::InvalidBuildIdentity)?;
            decoded[index] = (high << 4) | low;
        }
        Ok(Self(decoded))
    }

    /// Borrow the exact decoded SHA-256 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Consume the identity and return its decoded SHA-256 bytes.
    #[must_use]
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for SealedAtomicBuildIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SealedAtomicBuildIdentity")
            .field(&format_args!("{self}"))
            .finish()
    }
}

impl fmt::Display for SealedAtomicBuildIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Parsed identity state carried by a component marker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AtomicBuildIdentity {
    /// Development build with no release authority.
    UnsealedDevelopment,
    /// Release build carrying an exact decoded process-family authority.
    Sealed(SealedAtomicBuildIdentity),
}

impl AtomicBuildIdentity {
    fn from_marker_value(value: &str) -> Result<Self, AtomicComponentIdentityError> {
        if value == UNSEALED_BUILD_ID {
            return Ok(Self::UnsealedDevelopment);
        }
        SealedAtomicBuildIdentity::from_lower_hex(value).map(Self::Sealed)
    }

    /// Require a sealed identity, rejecting development markers explicitly.
    pub const fn require_sealed(
        self,
    ) -> Result<SealedAtomicBuildIdentity, AtomicComponentIdentityError> {
        match self {
            Self::Sealed(identity) => Ok(identity),
            Self::UnsealedDevelopment => {
                Err(AtomicComponentIdentityError::UnsealedDevelopmentBuild)
            }
        }
    }
}

impl fmt::Display for AtomicBuildIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsealedDevelopment => formatter.write_str(UNSEALED_BUILD_ID),
            Self::Sealed(identity) => identity.fmt(formatter),
        }
    }
}

/// Finite marker fields whose tokens share the same ASCII grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicComponentMarkerField {
    /// Cargo target triple.
    Target,
    /// Cargo build profile.
    Profile,
    /// Workspace package version.
    Version,
}

impl fmt::Display for AtomicComponentMarkerField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Target => "target",
            Self::Profile => "profile",
            Self::Version => "version",
        })
    }
}

/// Fail-closed errors for marker construction, parsing, and sealing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicComponentIdentityError {
    /// A build ID was not exactly 64 lowercase hexadecimal characters.
    InvalidBuildIdentity,
    /// A target, profile, or version token violated the marker grammar.
    InvalidMarkerToken(AtomicComponentMarkerField),
    /// A marker did not have the exact v1 prefix, field count, or terminator.
    InvalidMarkerShape,
    /// A component role token was not in the closed role set.
    InvalidComponentRole,
    /// A valid marker named a different component role.
    UnexpectedComponentRole {
        /// Role required by the consuming process.
        expected: AtomicComponentRole,
        /// Role actually carried by the marker.
        actual: AtomicComponentRole,
    },
    /// A development marker was presented where sealed authority was required.
    UnsealedDevelopmentBuild,
    /// A Cargo build-script input was not valid Unicode.
    EnvironmentNotUnicode(&'static str),
}

impl fmt::Display for AtomicComponentIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBuildIdentity => formatter.write_str(
                "atomic build identity must be exactly 64 lowercase hexadecimal characters",
            ),
            Self::InvalidMarkerToken(field) => write!(
                formatter,
                "atomic component {field} must be a non-empty ASCII token"
            ),
            Self::InvalidMarkerShape => {
                formatter.write_str("atomic component marker does not match the exact v1 grammar")
            }
            Self::InvalidComponentRole => {
                formatter.write_str("atomic component marker names an unsupported process role")
            }
            Self::UnexpectedComponentRole { expected, actual } => write!(
                formatter,
                "atomic component marker role mismatch: expected {expected}, found {actual}"
            ),
            Self::UnsealedDevelopmentBuild => formatter.write_str(
                "unsealed development build has no atomic process-family identity authority",
            ),
            Self::EnvironmentNotUnicode(variable) => {
                write!(formatter, "{variable} must be valid UTF-8")
            }
        }
    }
}

impl Error for AtomicComponentIdentityError {}

/// Fully validated fields borrowed from one exact-role v1 component marker.
///
/// The target, profile, and version are descriptive build metadata. Only a
/// [`SealedAtomicBuildIdentity`] returned by [`Self::require_sealed`] is build
/// authority.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ParsedAtomicComponentMarker<'a> {
    build_identity: AtomicBuildIdentity,
    role: AtomicComponentRole,
    target: &'a str,
    profile: &'a str,
    version: &'a str,
}

impl<'a> ParsedAtomicComponentMarker<'a> {
    /// Preserve the explicit sealed-versus-development identity state.
    #[must_use]
    pub const fn build_identity(self) -> AtomicBuildIdentity {
        self.build_identity
    }

    /// Return the validated process role.
    #[must_use]
    pub const fn role(self) -> AtomicComponentRole {
        self.role
    }

    /// Return the validated target triple metadata.
    #[must_use]
    pub const fn target(self) -> &'a str {
        self.target
    }

    /// Return the validated Cargo profile metadata.
    #[must_use]
    pub const fn profile(self) -> &'a str {
        self.profile
    }

    /// Return the validated package-version metadata.
    #[must_use]
    pub const fn version(self) -> &'a str {
        self.version
    }

    /// Require sealed build authority while retaining all validated metadata.
    pub const fn require_sealed(
        self,
    ) -> Result<ParsedSealedAtomicComponentMarker<'a>, AtomicComponentIdentityError> {
        let build_identity = match self.build_identity.require_sealed() {
            Ok(identity) => identity,
            Err(error) => return Err(error),
        };
        Ok(ParsedSealedAtomicComponentMarker {
            build_identity,
            role: self.role,
            target: self.target,
            profile: self.profile,
            version: self.version,
        })
    }
}

/// Fully validated fields borrowed from one exact-role sealed v1 marker.
///
/// Construction is private so this type can only exist after canonical
/// lowercase-hex decoding and exact role validation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ParsedSealedAtomicComponentMarker<'a> {
    build_identity: SealedAtomicBuildIdentity,
    role: AtomicComponentRole,
    target: &'a str,
    profile: &'a str,
    version: &'a str,
}

impl<'a> ParsedSealedAtomicComponentMarker<'a> {
    /// Return the decoded 32-byte build authority.
    #[must_use]
    pub const fn build_identity(self) -> SealedAtomicBuildIdentity {
        self.build_identity
    }

    /// Return the validated process role.
    #[must_use]
    pub const fn role(self) -> AtomicComponentRole {
        self.role
    }

    /// Return the validated target triple metadata.
    #[must_use]
    pub const fn target(self) -> &'a str {
        self.target
    }

    /// Return the validated Cargo profile metadata.
    #[must_use]
    pub const fn profile(self) -> &'a str {
        self.profile
    }

    /// Return the validated package-version metadata.
    #[must_use]
    pub const fn version(self) -> &'a str {
        self.version
    }
}

/// Construct one canonical v1 marker without reading process environment.
pub fn atomic_component_marker(
    build_identity: AtomicBuildIdentity,
    role: AtomicComponentRole,
    target: &str,
    profile: &str,
    version: &str,
) -> Result<String, AtomicComponentIdentityError> {
    validate_marker_token(target, AtomicComponentMarkerField::Target)?;
    validate_marker_token(profile, AtomicComponentMarkerField::Profile)?;
    validate_marker_token(version, AtomicComponentMarkerField::Version)?;
    Ok(format!(
        "{ATOMIC_COMPONENT_MARKER_PREFIX}{build_identity}:{role}:{target}:{profile}:{version};"
    ))
}

/// Parse one marker for an exact process role while preserving explicit
/// development-vs-sealed state.
pub fn parse_atomic_component_marker(
    marker: &str,
    expected_role: AtomicComponentRole,
) -> Result<AtomicBuildIdentity, AtomicComponentIdentityError> {
    Ok(parse_atomic_component_marker_details(marker, expected_role)?.build_identity())
}

/// Parse every validated field from one marker for an exact process role.
///
/// Callers that serialize a release manifest should use this API instead of
/// reparsing the marker grammar themselves.
pub fn parse_atomic_component_marker_details(
    marker: &str,
    expected_role: AtomicComponentRole,
) -> Result<ParsedAtomicComponentMarker<'_>, AtomicComponentIdentityError> {
    let payload = marker
        .strip_prefix(ATOMIC_COMPONENT_MARKER_PREFIX)
        .and_then(|value| value.strip_suffix(';'))
        .ok_or(AtomicComponentIdentityError::InvalidMarkerShape)?;
    let mut fields = payload.split(':');
    let build_identity = fields
        .next()
        .ok_or(AtomicComponentIdentityError::InvalidMarkerShape)?;
    let role = fields
        .next()
        .ok_or(AtomicComponentIdentityError::InvalidMarkerShape)?;
    let target = fields
        .next()
        .ok_or(AtomicComponentIdentityError::InvalidMarkerShape)?;
    let profile = fields
        .next()
        .ok_or(AtomicComponentIdentityError::InvalidMarkerShape)?;
    let version = fields
        .next()
        .ok_or(AtomicComponentIdentityError::InvalidMarkerShape)?;
    if fields.next().is_some() {
        return Err(AtomicComponentIdentityError::InvalidMarkerShape);
    }

    let actual_role = AtomicComponentRole::from_str(role)?;
    if actual_role != expected_role {
        return Err(AtomicComponentIdentityError::UnexpectedComponentRole {
            expected: expected_role,
            actual: actual_role,
        });
    }
    validate_marker_token(target, AtomicComponentMarkerField::Target)?;
    validate_marker_token(profile, AtomicComponentMarkerField::Profile)?;
    validate_marker_token(version, AtomicComponentMarkerField::Version)?;
    Ok(ParsedAtomicComponentMarker {
        build_identity: AtomicBuildIdentity::from_marker_value(build_identity)?,
        role: actual_role,
        target,
        profile,
        version,
    })
}

/// Parse one exact-role marker and require a decoded 32-byte sealed identity.
pub fn parse_sealed_atomic_component_marker(
    marker: &str,
    expected_role: AtomicComponentRole,
) -> Result<SealedAtomicBuildIdentity, AtomicComponentIdentityError> {
    Ok(parse_sealed_atomic_component_marker_details(marker, expected_role)?.build_identity())
}

/// Parse every validated field from one exact-role marker and require sealed
/// build authority.
pub fn parse_sealed_atomic_component_marker_details(
    marker: &str,
    expected_role: AtomicComponentRole,
) -> Result<ParsedSealedAtomicComponentMarker<'_>, AtomicComponentIdentityError> {
    parse_atomic_component_marker_details(marker, expected_role)?.require_sealed()
}

/// Build and emit the Cargo environment marker for one process role.
///
/// `FT_ATOMIC_BUILD_IDENTITY` may be absent for ordinary development builds;
/// when it is present it must be a canonical sealed identity. Supplying the
/// literal `unsealed` is rejected so release automation cannot disguise an
/// explicit value as the implicit development state.
pub fn emit_cargo_atomic_component_marker(
    role: AtomicComponentRole,
) -> Result<String, AtomicComponentIdentityError> {
    let build_identity = match std::env::var("FT_ATOMIC_BUILD_IDENTITY") {
        Ok(value) => {
            AtomicBuildIdentity::Sealed(SealedAtomicBuildIdentity::from_lower_hex(&value)?)
        }
        Err(std::env::VarError::NotPresent) => AtomicBuildIdentity::UnsealedDevelopment,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(AtomicComponentIdentityError::EnvironmentNotUnicode(
                "FT_ATOMIC_BUILD_IDENTITY",
            ));
        }
    };
    let target = read_build_environment("TARGET", "unknown")?;
    let version = read_build_environment("CARGO_PKG_VERSION", "unknown")?;
    let profile = match std::env::var("FT_ATOMIC_BUILD_PROFILE") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => read_build_environment("PROFILE", "unknown")?,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(AtomicComponentIdentityError::EnvironmentNotUnicode(
                "FT_ATOMIC_BUILD_PROFILE",
            ));
        }
    };
    let marker = atomic_component_marker(build_identity, role, &target, &profile, &version)?;
    println!("cargo:rustc-env=FT_ATOMIC_COMPONENT_MARKER={marker}");
    println!("cargo:rerun-if-env-changed=FT_ATOMIC_BUILD_IDENTITY");
    println!("cargo:rerun-if-env-changed=FT_ATOMIC_BUILD_PROFILE");
    Ok(marker)
}

fn read_build_environment(
    variable: &'static str,
    missing_value: &str,
) -> Result<String, AtomicComponentIdentityError> {
    match std::env::var(variable) {
        Ok(value) => Ok(value),
        Err(std::env::VarError::NotPresent) => Ok(missing_value.to_owned()),
        Err(std::env::VarError::NotUnicode(_)) => Err(
            AtomicComponentIdentityError::EnvironmentNotUnicode(variable),
        ),
    }
}

fn validate_marker_token(
    value: &str,
    field: AtomicComponentMarkerField,
) -> Result<(), AtomicComponentIdentityError> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
    {
        return Ok(());
    }
    Err(AtomicComponentIdentityError::InvalidMarkerToken(field))
}

const fn decode_lower_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEALED_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const SEALED_BYTES: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];

    #[test]
    fn sealed_identity_decodes_exactly_thirty_two_bytes() {
        let identity = SealedAtomicBuildIdentity::from_lower_hex(SEALED_HEX).unwrap();
        assert_eq!(identity.into_bytes(), SEALED_BYTES);
        assert_eq!(identity.to_string(), SEALED_HEX);
    }

    #[test]
    fn existing_component_marker_bytes_and_roles_do_not_drift() {
        let identity = AtomicBuildIdentity::Sealed(
            SealedAtomicBuildIdentity::from_lower_hex(SEALED_HEX).unwrap(),
        );
        for (role, expected_role) in [
            (AtomicComponentRole::Ft, "ft"),
            (AtomicComponentRole::FrankenTermGui, "frankenterm-gui"),
            (
                AtomicComponentRole::FrankenTermMuxServer,
                "frankenterm-mux-server",
            ),
            (
                AtomicComponentRole::FrankenTermPtyGuardian,
                "frankenterm-pty-guardian",
            ),
        ] {
            let marker = atomic_component_marker(
                identity,
                role,
                "aarch64-apple-darwin",
                "release-interactive",
                "0.15.1",
            )
            .unwrap();
            assert_eq!(
                marker,
                format!(
                    "FT_ATOMIC_COMPONENT_IDENTITY_V1:{SEALED_HEX}:{expected_role}:aarch64-apple-darwin:release-interactive:0.15.1;"
                )
            );
            assert_eq!(
                parse_sealed_atomic_component_marker(&marker, role).unwrap(),
                identity.require_sealed().unwrap()
            );
        }
    }

    #[test]
    fn unsealed_development_marker_never_becomes_runtime_authority() {
        let marker = atomic_component_marker(
            AtomicBuildIdentity::UnsealedDevelopment,
            AtomicComponentRole::FrankenTermPtyGuardian,
            "aarch64-apple-darwin",
            "debug",
            "0.15.1",
        )
        .unwrap();
        assert_eq!(
            parse_atomic_component_marker(&marker, AtomicComponentRole::FrankenTermPtyGuardian)
                .unwrap(),
            AtomicBuildIdentity::UnsealedDevelopment
        );
        assert_eq!(
            parse_sealed_atomic_component_marker(
                &marker,
                AtomicComponentRole::FrankenTermPtyGuardian
            ),
            Err(AtomicComponentIdentityError::UnsealedDevelopmentBuild)
        );
    }

    #[test]
    fn sealed_marker_details_bind_every_manifest_field() {
        let valid = format!(
            "FT_ATOMIC_COMPONENT_IDENTITY_V1:{SEALED_HEX}:frankenterm-pty-guardian:aarch64-apple-darwin:release-interactive:0.15.1;"
        );
        let parsed = parse_sealed_atomic_component_marker_details(
            &valid,
            AtomicComponentRole::FrankenTermPtyGuardian,
        )
        .unwrap();
        assert_eq!(parsed.build_identity().to_string(), SEALED_HEX);
        assert_eq!(parsed.role(), AtomicComponentRole::FrankenTermPtyGuardian);
        assert_eq!(parsed.target(), "aarch64-apple-darwin");
        assert_eq!(parsed.profile(), "release-interactive");
        assert_eq!(parsed.version(), "0.15.1");

        for (from, to) in [
            ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu"),
            ("release-interactive", "release-perf"),
            ("0.15.1", "0.15.2"),
        ] {
            let mutation = valid.replacen(from, to, 1);
            let mutated = parse_sealed_atomic_component_marker_details(
                &mutation,
                AtomicComponentRole::FrankenTermPtyGuardian,
            )
            .unwrap();
            assert_ne!(mutated, parsed, "metadata mutation {from:?} was not bound");
        }
    }

    #[test]
    fn every_hex_nibble_is_bound_into_the_decoded_identity() {
        let baseline = SealedAtomicBuildIdentity::from_lower_hex(SEALED_HEX).unwrap();
        for index in 0..SEALED_HEX.len() {
            let mut mutated = SEALED_HEX.as_bytes().to_vec();
            mutated[index] = if mutated[index] == b'f' { b'e' } else { b'f' };
            let mutated = String::from_utf8(mutated).unwrap();
            let decoded = SealedAtomicBuildIdentity::from_lower_hex(&mutated).unwrap();
            assert_ne!(decoded, baseline, "hex nibble {index} was not bound");
        }
    }

    #[test]
    fn marker_grammar_and_expected_role_fail_closed_under_mutation() {
        let valid = format!(
            "FT_ATOMIC_COMPONENT_IDENTITY_V1:{SEALED_HEX}:frankenterm-pty-guardian:aarch64-apple-darwin:release-interactive:0.15.1;"
        );
        let mutations = [
            valid.replacen("IDENTITY_V1", "IDENTITY_V2", 1),
            valid.replacen(SEALED_HEX, &SEALED_HEX.to_uppercase(), 1),
            valid.trim_end_matches(';').to_owned(),
            valid.replacen("aarch64-apple-darwin", "aarch64:apple-darwin", 1),
            valid.replacen("release-interactive", "", 1),
            valid.replacen("0.15.1", "0.15.1\nforged", 1),
            valid.replacen(":0.15.1;", ":extra:0.15.1;", 1),
        ];
        for mutation in mutations {
            assert!(
                parse_atomic_component_marker(
                    &mutation,
                    AtomicComponentRole::FrankenTermPtyGuardian
                )
                .is_err(),
                "accepted mutated marker {mutation:?}"
            );
        }
        assert_eq!(
            parse_atomic_component_marker(&valid, AtomicComponentRole::FrankenTermMuxServer),
            Err(AtomicComponentIdentityError::UnexpectedComponentRole {
                expected: AtomicComponentRole::FrankenTermMuxServer,
                actual: AtomicComponentRole::FrankenTermPtyGuardian,
            })
        );
    }

    #[test]
    fn uppercase_short_long_and_explicit_unsealed_values_are_not_sealed_ids() {
        let invalid = [
            SEALED_HEX.to_uppercase(),
            SEALED_HEX[..63].to_owned(),
            format!("{SEALED_HEX}0"),
            UNSEALED_BUILD_ID.to_owned(),
        ];
        for invalid in invalid {
            assert_eq!(
                SealedAtomicBuildIdentity::from_lower_hex(&invalid),
                Err(AtomicComponentIdentityError::InvalidBuildIdentity),
                "accepted invalid sealed identity {invalid:?}"
            );
        }
    }
}
