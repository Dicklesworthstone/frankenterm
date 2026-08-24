use crate::{Result, ensure, format_err};
use core::hash::{Hash, Hasher};
use frankenterm_dynamic::{
    Error as DynamicError, FromDynamic, FromDynamicOptions, ToDynamic, Value,
};
#[cfg(feature = "use_serde")]
use serde::{ser::SerializeStruct, Deserialize, Deserializer, Serialize, Serializer};
use zeroize::{Zeroize, Zeroizing};

use crate::allocate::*;

#[cfg(test)]
extern crate std;

#[cfg(test)]
std::thread_local! {
    static HYPERLINK_WIPE_INVOCATIONS: core::cell::Cell<usize> =
        const { core::cell::Cell::new(0) };
    static GUARDED_STRING_WIPE_INVOCATIONS: core::cell::Cell<usize> =
        const { core::cell::Cell::new(0) };
}

fn percent_encode_byte(f: &mut core::fmt::Formatter, byte: u8) -> core::fmt::Result {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    write!(
        f,
        "%{}{}",
        HEX[(byte >> 4) as usize] as char,
        HEX[(byte & 0x0f) as usize] as char
    )
}

fn should_encode_param_byte(byte: u8) -> bool {
    !matches!(byte, 0x21..=0x7e) || matches!(byte, b'%' | b':' | b'=' | b';')
}

fn should_encode_uri_byte(byte: u8) -> bool {
    !matches!(byte, 0x21..=0x7e) || matches!(byte, b'%' | b';')
}

fn write_percent_encoded(
    f: &mut core::fmt::Formatter,
    value: &str,
    should_encode: fn(u8) -> bool,
) -> core::fmt::Result {
    for byte in value.bytes() {
        if should_encode(byte) {
            percent_encode_byte(f, byte)?;
        } else {
            write!(f, "{}", byte as char)?;
        }
    }
    Ok(())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_percent_escapes(input: &[u8]) -> Result<Zeroizing<String>> {
    // Percent decoding cannot expand its input, so this reservation prevents
    // a plaintext-bearing scratch reallocation.  The guard is installed at
    // the allocation site rather than after validation.
    let mut out = Zeroizing::new(Vec::with_capacity(input.len()));
    let mut idx = 0;
    while idx < input.len() {
        if input[idx] == b'%' && idx + 2 < input.len() {
            if let (Some(hi), Some(lo)) = (hex_value(input[idx + 1]), hex_value(input[idx + 2])) {
                out.push((hi << 4) | lo);
                idx += 3;
                continue;
            }
        }
        out.push(input[idx]);
        idx += 1;
    }

    match String::from_utf8(core::mem::take(&mut *out)) {
        Ok(text) => Ok(Zeroizing::new(text)),
        Err(error) => {
            let utf8_error = error.utf8_error();
            let valid_up_to = utf8_error.valid_up_to();
            let error_len = utf8_error.error_len();
            let mut rejected = Zeroizing::new(error.into_bytes());
            rejected.zeroize();
            Err(crate::error::StringWrap(format!(
                "percent-decoded hyperlink field is not valid UTF-8 at byte {valid_up_to} (error_len={error_len:?})"
            ))
            .into())
        }
    }
}

/// A construction-time owner for semantic hyperlink text.
///
/// `Hyperlink` deliberately retains ordinary `String` fields for its public
/// API and wire shape.  This private type closes the interval between decoding
/// a string and successfully installing it in a Drop-hardened `Hyperlink`.
#[derive(Debug)]
struct GuardedString(Zeroizing<String>);

impl GuardedString {
    fn new(text: String) -> Self {
        Self(Zeroizing::new(text))
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn take(&mut self) -> String {
        core::mem::take(&mut *self.0)
    }
}

impl Drop for GuardedString {
    fn drop(&mut self) {
        self.0.zeroize();

        #[cfg(test)]
        GUARDED_STRING_WIPE_INVOCATIONS.with(|count| count.set(count.get().saturating_add(1)));
    }
}

impl PartialEq for GuardedString {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for GuardedString {}

impl PartialOrd for GuardedString {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GuardedString {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for GuardedString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for GuardedString {
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

impl FromDynamic for GuardedString {
    fn from_dynamic(
        value: &Value,
        options: FromDynamicOptions,
    ) -> core::result::Result<Self, DynamicError> {
        String::from_dynamic(value, options).map(Self::new)
    }
}

type GuardedParams = HashMap<GuardedString, GuardedString>;

#[cfg(feature = "use_serde")]
#[derive(Deserialize)]
#[serde(rename = "Hyperlink")]
struct GuardedHyperlink {
    params: GuardedParams,
    uri: GuardedString,
    implicit: bool,
}

#[derive(Debug, PartialEq, Eq, ToDynamic)]
pub struct Hyperlink {
    params: HashMap<String, String>,
    uri: String,
    /// If the link was produced by an implicit or matching rule,
    /// this field will be set to true.
    implicit: bool,
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for Hyperlink {
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let GuardedHyperlink {
            params,
            uri,
            implicit,
        } = GuardedHyperlink::deserialize(deserializer)?;
        Ok(Self::from_guarded_parts(uri, params, implicit))
    }
}

fn dynamic_field_error(field_name: &'static str, error: DynamicError) -> DynamicError {
    // Do not use `Error::field_context` here: for small objects that helper
    // formats the complete source object, copying the URI and parameters into
    // a new, unguarded diagnostic String.  The conversion errors below carry
    // type information, so this retains useful context without terminal text.
    DynamicError::Message(format!(
        "Error processing Hyperlink::{field_name}: {error:#}"
    ))
}

impl FromDynamic for Hyperlink {
    fn from_dynamic(
        value: &Value,
        options: FromDynamicOptions,
    ) -> core::result::Result<Self, DynamicError> {
        let Value::Object(object) = value else {
            return Err(DynamicError::NoConversion {
                source_type: value.variant_name().to_string(),
                dest_type: "Hyperlink",
            });
        };

        DynamicError::raise_unknown_fields(
            options,
            "Hyperlink",
            object,
            Self::possible_field_names(),
        )?;
        let params = GuardedParams::from_dynamic(
            object.get_by_str("params").unwrap_or(&Value::Null),
            options,
        )
        .map_err(|error| dynamic_field_error("params", error))?;
        let uri = GuardedString::from_dynamic(
            object.get_by_str("uri").unwrap_or(&Value::Null),
            options,
        )
        .map_err(|error| dynamic_field_error("uri", error))?;
        let implicit = bool::from_dynamic(
            object.get_by_str("implicit").unwrap_or(&Value::Null),
            options,
        )
        .map_err(|error| dynamic_field_error("implicit", error))?;

        Ok(Self::from_guarded_parts(uri, params, implicit))
    }
}

impl Clone for Hyperlink {
    fn clone(&self) -> Self {
        // A derived clone builds raw params before the URI; a later allocation
        // failure would then drop those strings without a Hyperlink owner.
        // Install each clone in the final Drop-hardened owner as it succeeds.
        let mut cloned = Self::empty(self.implicit);
        let mut uri = GuardedString::new(self.uri.clone());
        cloned.uri = uri.take();
        #[cfg(feature = "std")]
        cloned.params.reserve(self.params.len());
        for (key, value) in &self.params {
            let mut key = GuardedString::new(key.clone());
            let mut value = GuardedString::new(value.clone());
            cloned.params.insert(key.take(), value.take());
        }
        cloned
    }
}

#[cfg(feature = "use_serde")]
impl Serialize for Hyperlink {
    fn serialize<S>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // HashMap iteration order is randomized. Sorting here preserves the
        // existing wire shape while giving terminal checkpoints and render
        // snapshots one canonical byte representation for equivalent links.
        let params: BTreeMap<&str, &str> = self
            .params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let mut state = serializer.serialize_struct("Hyperlink", 3)?;
        state.serialize_field("params", &params)?;
        state.serialize_field("uri", &self.uri)?;
        state.serialize_field("implicit", &self.implicit)?;
        state.end()
    }
}

impl Hyperlink {
    fn empty(implicit: bool) -> Self {
        Self {
            params: HashMap::new(),
            uri: String::new(),
            implicit,
        }
    }

    fn from_guarded_parts(
        mut uri: GuardedString,
        mut params: GuardedParams,
        implicit: bool,
    ) -> Self {
        // Establish the final Drop owner before transferring any plaintext out
        // of its guards.  For std HashMap, reserving before extraction also
        // ensures insertion cannot reallocate the plaintext-bearing map.
        let mut link = Self::empty(implicit);
        link.uri = uri.take();
        #[cfg(feature = "std")]
        link.params.reserve(params.len());
        for (mut key, mut value) in core::mem::take(&mut params) {
            link.params.insert(key.take(), value.take());
        }
        link
    }

    pub const fn possible_field_names() -> &'static [&'static str] {
        &["params", "uri", "implicit"]
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        self.uri.hash(hasher);
        if self.params.len() <= 1 {
            for (k, v) in &self.params {
                k.hash(hasher);
                v.hash(hasher);
            }
        } else {
            let mut params = Vec::with_capacity(self.params.len());
            params.extend(self.params.iter());
            params.sort_unstable_by(|(ka, va), (kb, vb)| ka.cmp(kb).then_with(|| va.cmp(vb)));
            for (k, v) in params {
                k.hash(hasher);
                v.hash(hasher);
            }
        }
        self.implicit.hash(hasher);
    }

    pub fn params(&self) -> &HashMap<String, String> {
        &self.params
    }

    pub fn new<S: Into<String>>(uri: S) -> Self {
        let mut link = Self::empty(false);
        let mut uri = GuardedString::new(uri.into());
        link.uri = uri.take();
        link
    }

    #[inline]
    pub fn is_implicit(&self) -> bool {
        self.implicit
    }

    pub fn new_implicit<S: Into<String>>(uri: S) -> Self {
        let mut link = Self::empty(true);
        let mut uri = GuardedString::new(uri.into());
        link.uri = uri.take();
        link
    }

    pub fn new_with_id<S: Into<String>, S2: Into<String>>(uri: S, id: S2) -> Self {
        let mut link = Self::new(uri);
        let mut id = GuardedString::new(id.into());
        #[cfg(feature = "std")]
        link.params.reserve(1);
        link.params.insert("id".into(), id.take());
        link
    }

    pub fn new_with_params<S: Into<String>>(uri: S, params: HashMap<String, String>) -> Self {
        // Install caller-owned parameters in a Drop-hardened owner before the
        // URI conversion can fail or unwind.
        let mut link = Self {
            uri: String::new(),
            params,
            implicit: false,
        };
        let mut uri = GuardedString::new(uri.into());
        link.uri = uri.take();
        link
    }

    /// Reconstruct a hyperlink from a capability-free semantic checkpoint.
    ///
    /// Ordinary OSC parsing should continue to use `new_with_params`; this
    /// constructor exists because implicit-rule provenance affects whether a
    /// restored line may be rescanned and must therefore survive persistence.
    pub fn new_with_params_and_implicit<S: Into<String>>(
        uri: S,
        params: HashMap<String, String>,
        implicit: bool,
    ) -> Self {
        let mut link = Self {
            uri: String::new(),
            params,
            implicit,
        };
        let mut uri = GuardedString::new(uri.into());
        link.uri = uri.take();
        link
    }

    pub fn parse(osc: &[&[u8]]) -> Result<Option<Hyperlink>> {
        ensure!(osc.len() == 3, "wrong param count");
        if osc[1].is_empty() && osc[2].is_empty() {
            // Clearing current hyperlink
            Ok(None)
        } else {
            let mut uri = decode_percent_escapes(osc[2])?;
            let param_count = if osc[1].is_empty() {
                0
            } else {
                osc[1].split(|byte| *byte == b':').count()
            };
            let mut guarded_params: Vec<(Zeroizing<String>, Zeroizing<String>)> =
                Vec::with_capacity(param_count);
            if !osc[1].is_empty() {
                for pair in osc[1].split(|byte| *byte == b':') {
                    let separator = pair
                        .iter()
                        .position(|byte| *byte == b'=')
                        .ok_or_else(|| format_err!("bad params"))?;
                    let key = decode_percent_escapes(&pair[..separator])?;
                    let value = decode_percent_escapes(&pair[separator + 1..])?;

                    if let Some((_, prior_value)) = guarded_params
                        .iter_mut()
                        .find(|(prior_key, _)| prior_key.as_str() == key.as_str())
                    {
                        prior_value.zeroize();
                        *prior_value = value;
                    } else {
                        guarded_params.push((key, value));
                    }
                }
            }

            // All recoverable validation is complete.  Construct the final
            // Drop-hardened owner before transferring any guarded string into its
            // raw String fields, so a later unwind cannot bypass Hyperlink's
            // wipe contract.
            let mut link = Hyperlink::new(core::mem::take(&mut *uri));
            #[cfg(feature = "std")]
            link.params.reserve(guarded_params.len());
            for (key, value) in &mut guarded_params {
                link.params.insert(
                    core::mem::take(&mut **key),
                    core::mem::take(&mut **value),
                );
            }

            Ok(Some(link))
        }
    }

    fn wipe_owned_text(&mut self) {
        self.uri.zeroize();
        // `HashMap` is a `BTreeMap` alias in the no_std build.  Taking and
        // consuming the map is the common, allocation-free full-drain path for
        // both representations and gives us owned access to every string.
        for (mut key, mut value) in core::mem::take(&mut self.params) {
            key.zeroize();
            value.zeroize();
        }

        #[cfg(test)]
        HYPERLINK_WIPE_INVOCATIONS.with(|count| count.set(count.get().saturating_add(1)));
    }
}

impl Drop for Hyperlink {
    fn drop(&mut self) {
        self.wipe_owned_text();
    }
}

impl zeroize::ZeroizeOnDrop for Hyperlink {}

impl core::fmt::Display for Hyperlink {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "8;")?;
        let mut params = Vec::with_capacity(self.params.len());
        params.extend(self.params.iter());
        params.sort_unstable_by(|(left_key, left_value), (right_key, right_value)| {
            left_key
                .cmp(right_key)
                .then_with(|| left_value.cmp(right_value))
        });
        for (idx, (k, v)) in params.into_iter().enumerate() {
            if idx > 0 {
                write!(f, ":")?;
            }
            write_percent_encoded(f, k, should_encode_param_byte)?;
            write!(f, "=")?;
            write_percent_encoded(f, v, should_encode_param_byte)?;
        }
        write!(f, ";")?;
        write_percent_encoded(f, &self.uri, should_encode_uri_byte)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    fn hyperlink_wipe_invocations() -> usize {
        HYPERLINK_WIPE_INVOCATIONS.with(|count| count.get())
    }

    fn guarded_string_wipe_invocations() -> usize {
        GUARDED_STRING_WIPE_INVOCATIONS.with(|count| count.get())
    }

    #[test]
    fn new_creates_non_implicit_link() {
        let link = Hyperlink::new("https://example.com");
        assert_eq!(link.uri(), "https://example.com");
        assert!(!link.is_implicit());
        assert!(link.params().is_empty());
    }

    #[test]
    fn new_implicit_creates_implicit_link() {
        let link = Hyperlink::new_implicit("https://example.com");
        assert_eq!(link.uri(), "https://example.com");
        assert!(link.is_implicit());
        assert!(link.params().is_empty());
    }

    #[test]
    fn display_orders_params_canonically() {
        let mut first = HashMap::new();
        first.insert("zeta".to_string(), "last".to_string());
        first.insert("alpha".to_string(), "first".to_string());
        let mut second = HashMap::new();
        second.insert("alpha".to_string(), "first".to_string());
        second.insert("zeta".to_string(), "last".to_string());

        let first = Hyperlink::new_with_params("https://example.com", first);
        let second = Hyperlink::new_with_params("https://example.com", second);
        assert_eq!(first.to_string(), second.to_string());
        assert_eq!(
            first.to_string(),
            "8;alpha=first:zeta=last;https://example.com"
        );
    }

    #[test]
    fn new_with_id() {
        let link = Hyperlink::new_with_id("https://example.com", "link1");
        assert_eq!(link.uri(), "https://example.com");
        assert!(!link.is_implicit());
        assert_eq!(link.params().get("id"), Some(&"link1".to_string()));
    }

    #[test]
    fn new_with_params() {
        let mut params = HashMap::new();
        params.insert("id".to_string(), "myid".to_string());
        params.insert("class".to_string(), "external".to_string());
        let link = Hyperlink::new_with_params("https://example.com", params);
        assert_eq!(link.uri(), "https://example.com");
        assert_eq!(link.params().len(), 2);
        assert_eq!(link.params().get("id"), Some(&"myid".to_string()));
        assert_eq!(link.params().get("class"), Some(&"external".to_string()));
    }

    #[test]
    fn equality() {
        let a = Hyperlink::new("https://example.com");
        let b = Hyperlink::new("https://example.com");
        assert_eq!(a, b);

        let c = Hyperlink::new("https://other.com");
        assert_ne!(a, c);
    }

    #[test]
    fn implicit_vs_explicit_not_equal() {
        let a = Hyperlink::new("https://example.com");
        let b = Hyperlink::new_implicit("https://example.com");
        assert_ne!(a, b);
    }

    #[test]
    fn clone() {
        let link = Hyperlink::new_with_id("https://example.com", "id1");
        let source_uri = link.uri.as_ptr();
        let source_id = link.params.get("id").unwrap().as_ptr();
        let cloned = link.clone();
        assert_eq!(link, cloned);
        assert_ne!(source_uri, cloned.uri.as_ptr());
        assert_ne!(source_id, cloned.params.get("id").unwrap().as_ptr());

        drop(link);

        assert_eq!(cloned.uri(), "https://example.com");
        assert_eq!(cloned.params().get("id"), Some(&"id1".to_string()));
    }

    #[test]
    fn wipe_owned_text_clears_uri_and_every_param() {
        fn require_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        require_zeroize_on_drop::<Hyperlink>();

        let mut params = HashMap::new();
        params.insert("identity".to_string(), "sensitive-value".to_string());
        params.insert("class".to_string(), "private".to_string());
        let mut link = Hyperlink::new_with_params("secret://terminal/session", params);
        let before = hyperlink_wipe_invocations();

        link.wipe_owned_text();

        assert!(link.uri().is_empty());
        assert!(link.params().is_empty());
        assert_eq!(hyperlink_wipe_invocations(), before + 1);
    }

    #[test]
    fn arc_only_runs_hyperlink_wipe_for_the_final_owner() {
        let mut params = HashMap::new();
        params.insert("identity".to_string(), "sensitive-value".to_string());
        params.insert("class".to_string(), "private".to_string());
        let first = Arc::new(Hyperlink::new_with_params(
            "secret://terminal/session",
            params,
        ));
        let retained = Arc::clone(&first);
        let before = hyperlink_wipe_invocations();

        drop(first);

        assert_eq!(hyperlink_wipe_invocations(), before);
        assert_eq!(retained.uri(), "secret://terminal/session");
        assert_eq!(retained.params().len(), 2);

        drop(retained);

        assert_eq!(hyperlink_wipe_invocations(), before + 1);
    }

    #[test]
    fn debug_format() {
        let link = Hyperlink::new("https://example.com");
        let dbg = format!("{:?}", link);
        assert!(dbg.contains("Hyperlink"));
        assert!(dbg.contains("https://example.com"));
    }

    #[test]
    fn display_no_params() {
        let link = Hyperlink::new("https://example.com");
        let display = format!("{}", link);
        assert!(display.starts_with("8;"));
        assert!(display.ends_with(";https://example.com"));
        // With no params: "8;;https://example.com"
        assert_eq!(display, "8;;https://example.com");
    }

    #[test]
    fn display_with_one_param() {
        let link = Hyperlink::new_with_id("https://example.com", "link1");
        let display = format!("{}", link);
        assert!(display.starts_with("8;"));
        assert!(display.contains("id=link1"));
        assert!(display.ends_with(";https://example.com"));
    }

    #[test]
    fn display_percent_encodes_osc8_param_and_uri_separators() {
        let mut params = HashMap::new();
        params.insert("id".to_string(), "a:b=c;d%".to_string());
        params.insert("class:name".to_string(), "external=docs".to_string());
        let link = Hyperlink::new_with_params("https://example.com/a;b?x=1%202", params);

        let display = format!("{}", link);
        assert_eq!(
            display,
            "8;class%3Aname=external%3Ddocs:id=a%3Ab%3Dc%3Bd%25;\
             https://example.com/a%3Bb?x=1%25202"
        );
        assert_eq!(
            display.split(';').count(),
            3,
            "Display must not emit raw OSC 8 semicolon separators inside fields"
        );
    }

    #[test]
    fn display_output_roundtrips_through_parse_with_reserved_chars() {
        let mut params = HashMap::new();
        params.insert("id".to_string(), "a:b=c;d%".to_string());
        params.insert("class:name".to_string(), "external=docs".to_string());
        let link = Hyperlink::new_with_params("https://example.com/a;b?x=1%202", params);

        let display = format!("{}", link);
        let osc: Vec<&[u8]> = display.split(';').map(str::as_bytes).collect();
        let parsed = Hyperlink::parse(&osc).unwrap().unwrap();
        assert_eq!(parsed, link);
    }

    #[test]
    fn parse_clear_link() {
        let osc: Vec<&[u8]> = vec![b"8", b"", b""];
        let result = Hyperlink::parse(&osc).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_simple_link() {
        let osc: Vec<&[u8]> = vec![b"8", b"", b"https://example.com"];
        let result = Hyperlink::parse(&osc).unwrap();
        assert!(result.is_some());
        let link = result.unwrap();
        assert_eq!(link.uri(), "https://example.com");
        assert!(link.params().is_empty());
    }

    #[test]
    fn parse_link_with_id_param() {
        let osc: Vec<&[u8]> = vec![b"8", b"id=mylink", b"https://example.com"];
        let result = Hyperlink::parse(&osc).unwrap();
        assert!(result.is_some());
        let link = result.unwrap();
        assert_eq!(link.uri(), "https://example.com");
        assert_eq!(link.params().get("id"), Some(&"mylink".to_string()));
    }

    #[test]
    fn parse_link_with_multiple_params() {
        let osc: Vec<&[u8]> = vec![b"8", b"id=link1:class=external", b"https://example.com"];
        let result = Hyperlink::parse(&osc).unwrap();
        assert!(result.is_some());
        let link = result.unwrap();
        assert_eq!(link.params().len(), 2);
        assert_eq!(link.params().get("id"), Some(&"link1".to_string()));
        assert_eq!(link.params().get("class"), Some(&"external".to_string()));
    }

    #[test]
    fn parse_duplicate_param_keeps_the_last_guarded_value() {
        let osc: Vec<&[u8]> = vec![
            b"8",
            b"id=first:id=second",
            b"https://example.com/private",
        ];

        let link = Hyperlink::parse(&osc).unwrap().unwrap();

        assert_eq!(link.params().len(), 1);
        assert_eq!(link.params().get("id"), Some(&"second".to_string()));
    }

    #[test]
    fn percent_decode_rejects_invalid_utf8_with_a_content_free_error() {
        let error = decode_percent_escapes(b"private-uri-%FF")
            .expect_err("decoded 0xff must be rejected as invalid UTF-8");
        let message = error.to_string();

        assert!(message.contains("not valid UTF-8"));
        assert!(message.contains("byte 12"));
        assert!(
            !message.contains("private-uri"),
            "UTF-8 errors must not retain or disclose rejected hyperlink bytes"
        );
    }

    #[test]
    fn parse_wrong_param_count() {
        let osc: Vec<&[u8]> = vec![b"8", b""];
        let result = Hyperlink::parse(&osc);
        assert!(result.is_err());
    }

    #[test]
    fn new_accepts_string_and_str() {
        let a = Hyperlink::new("https://example.com");
        let b = Hyperlink::new(String::from("https://example.com"));
        assert_eq!(a, b);
    }

    // Raw, unescaped separators remain invalid input. Display must
    // never emit this shape; it percent-encodes reserved bytes so the
    // output keeps exactly three OSC 8 fields.
    #[test]
    fn ft_mw1nw_parse_rejects_value_with_embedded_colon() {
        let osc: Vec<&[u8]> = vec![b"8", b"id=a:b", b"https://example.com"];
        let result = Hyperlink::parse(&osc);
        assert!(
            result.is_err(),
            "raw unescaped ':' must stay invalid; Display should emit %3A instead. \
             got Ok({result:?})"
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn compute_shape_hash_same_for_equal_links() {
        use std::collections::hash_map::DefaultHasher;
        let a = Hyperlink::new("https://example.com");
        let b = Hyperlink::new("https://example.com");
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        a.compute_shape_hash(&mut h1);
        b.compute_shape_hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[cfg(feature = "std")]
    #[test]
    fn compute_shape_hash_ignores_param_insertion_order() {
        use std::collections::hash_map::DefaultHasher;

        let mut params_a = HashMap::new();
        params_a.insert("id".to_string(), "link-1".to_string());
        params_a.insert("class".to_string(), "external".to_string());

        let mut params_b = HashMap::new();
        params_b.insert("class".to_string(), "external".to_string());
        params_b.insert("id".to_string(), "link-1".to_string());

        let a = Hyperlink::new_with_params("https://example.com", params_a);
        let b = Hyperlink::new_with_params("https://example.com", params_b);

        assert_eq!(a, b, "links with equal params must compare equal");

        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        a.compute_shape_hash(&mut h1);
        b.compute_shape_hash(&mut h2);
        assert_eq!(
            h1.finish(),
            h2.finish(),
            "shape hash must not depend on HashMap insertion order"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn serialization_is_canonical_across_param_insertion_order() {
        let mut params_a = HashMap::new();
        params_a.insert("zeta".to_string(), "last".to_string());
        params_a.insert("alpha".to_string(), "first".to_string());

        let mut params_b = HashMap::new();
        params_b.insert("alpha".to_string(), "first".to_string());
        params_b.insert("zeta".to_string(), "last".to_string());

        let a = Hyperlink::new_with_params("https://example.com", params_a);
        let b = Hyperlink::new_with_params("https://example.com", params_b);
        let encoded_a = serde_json::to_vec(&a).expect("serialize first hyperlink");
        let encoded_b = serde_json::to_vec(&b).expect("serialize second hyperlink");

        assert_eq!(encoded_a, encoded_b);
        assert_eq!(
            serde_json::from_slice::<Hyperlink>(&encoded_a).expect("deserialize hyperlink"),
            a
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn serde_failure_wipes_every_successfully_decoded_guarded_field() {
        let before = guarded_string_wipe_invocations();
        let result = serde_json::from_str::<Hyperlink>(
            r#"{"params":{"private-key":"private-value"},"uri":"secret://terminal/session","implicit":"not-a-bool"}"#,
        );

        assert!(result.is_err());
        assert!(
            guarded_string_wipe_invocations() >= before + 3,
            "the parameter key, value, and URI must all be wiped when a later field rejects"
        );
    }

    #[test]
    fn from_dynamic_failure_wipes_every_successfully_decoded_guarded_field() {
        let mut params = BTreeMap::new();
        params.insert(
            Value::String("private-key".to_string()),
            Value::String("private-value".to_string()),
        );
        let mut object = BTreeMap::new();
        object.insert(
            Value::String("params".to_string()),
            Value::Object(params.into()),
        );
        object.insert(
            Value::String("uri".to_string()),
            Value::String("secret://terminal/session".to_string()),
        );
        object.insert(
            Value::String("implicit".to_string()),
            Value::String("not-a-bool".to_string()),
        );
        let value = Value::Object(object.into());
        let before = guarded_string_wipe_invocations();

        let result = Hyperlink::from_dynamic(&value, FromDynamicOptions::default());

        assert!(result.is_err());
        assert!(
            guarded_string_wipe_invocations() >= before + 3,
            "dynamic parameter key, value, and URI guards must wipe on a later-field error"
        );
    }
}
