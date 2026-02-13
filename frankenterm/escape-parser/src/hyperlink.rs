use crate::{Result, ensure, format_err};
use core::hash::{Hash, Hasher};
use frankenterm_dynamic::{FromDynamic, ToDynamic};
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};

use crate::allocate::*;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub struct Hyperlink {
    params: HashMap<String, String>,
    uri: String,
    /// If the link was produced by an implicit or matching rule,
    /// this field will be set to true.
    implicit: bool,
}

impl Hyperlink {
    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        self.uri.hash(hasher);
        for (k, v) in &self.params {
            k.hash(hasher);
            v.hash(hasher);
        }
        self.implicit.hash(hasher);
    }

    pub fn params(&self) -> &HashMap<String, String> {
        &self.params
    }

    pub fn new<S: Into<String>>(uri: S) -> Self {
        Self {
            uri: uri.into(),
            params: HashMap::new(),
            implicit: false,
        }
    }

    #[inline]
    pub fn is_implicit(&self) -> bool {
        self.implicit
    }

    pub fn new_implicit<S: Into<String>>(uri: S) -> Self {
        Self {
            uri: uri.into(),
            params: HashMap::new(),
            implicit: true,
        }
    }

    pub fn new_with_id<S: Into<String>, S2: Into<String>>(uri: S, id: S2) -> Self {
        let mut params = HashMap::new();
        params.insert("id".into(), id.into());
        Self {
            uri: uri.into(),
            params,
            implicit: false,
        }
    }

    pub fn new_with_params<S: Into<String>>(uri: S, params: HashMap<String, String>) -> Self {
        Self {
            uri: uri.into(),
            params,
            implicit: false,
        }
    }

    pub fn parse(osc: &[&[u8]]) -> Result<Option<Hyperlink>> {
        ensure!(osc.len() == 3, "wrong param count");
        if osc[1].is_empty() && osc[2].is_empty() {
            // Clearing current hyperlink
            Ok(None)
        } else {
            let param_str = String::from_utf8(osc[1].to_vec())?;
            let uri = String::from_utf8(osc[2].to_vec())?;

            let mut params = HashMap::new();
            if !param_str.is_empty() {
                for pair in param_str.split(':') {
                    let mut iter = pair.splitn(2, '=');
                    let key = iter.next().ok_or_else(|| format_err!("bad params"))?;
                    let value = iter.next().ok_or_else(|| format_err!("bad params"))?;
                    params.insert(key.to_owned(), value.to_owned());
                }
            }

            Ok(Some(Hyperlink::new_with_params(uri, params)))
        }
    }
}

impl core::fmt::Display for Hyperlink {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "8;")?;
        for (idx, (k, v)) in self.params.iter().enumerate() {
            // TODO: protect against k, v containing : or =
            if idx > 0 {
                write!(f, ":")?;
            }
            write!(f, "{}={}", k, v)?;
        }
        // TODO: ensure that link.uri doesn't contain characters
        // outside the range 32-126.  Need to pull in a URI/URL
        // crate to help with this.
        write!(f, ";{}", self.uri)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let cloned = link.clone();
        assert_eq!(link, cloned);
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
}
