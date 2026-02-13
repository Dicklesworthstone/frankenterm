use crate::fromdynamic::{FromDynamicOptions, UnknownFieldAction};
use crate::object::Object;
use crate::value::Value;
#[cfg(feature = "std")]
use std::cell::RefCell;
#[cfg(feature = "std")]
use std::rc::Rc;
use thiserror::Error;

#[cfg(not(feature = "std"))]
extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::{format, vec};

#[cfg(feature = "std")]
pub trait WarningCollector {
    fn warn(&self, message: String);
}

#[cfg(feature = "std")]
thread_local! {
    static WARNING_COLLECTOR: RefCell<Option<Box<dyn WarningCollector>>> = RefCell::new(None);
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("`{}` is not a valid {} variant. {}", .variant_name, .type_name, Self::possible_matches(.variant_name, .possible))]
    InvalidVariantForType {
        variant_name: String,
        type_name: &'static str,
        possible: &'static [&'static str],
    },
    #[error("`{}` is not a valid {} field. {}", .field_name, .type_name, Self::possible_matches(.field_name, .possible))]
    UnknownFieldForStruct {
        field_name: String,
        type_name: &'static str,
        possible: &'static [&'static str],
    },
    #[error("{}", .0)]
    Message(String),
    #[error("Cannot coerce vec of size {} to array of size {}", .vec_size, .array_size)]
    ArraySizeMismatch { vec_size: usize, array_size: usize },
    #[error("Cannot convert `{}` to `{}`", .source_type, .dest_type)]
    NoConversion {
        source_type: String,
        dest_type: &'static str,
    },
    #[error("Expected char to be a string with a single character")]
    CharFromWrongSizedString,
    #[error("Expected a valid `{}` variant name as single key in object, but there are {} keys", .type_name, .num_keys)]
    IncorrectNumberOfEnumKeys {
        type_name: &'static str,
        num_keys: usize,
    },
    #[error("Error processing {}::{}: {:#}", .type_name, .field_name, .error)]
    ErrorInField {
        type_name: &'static str,
        field_name: &'static str,
        error: String,
    },
    #[error("Error processing {} (types: {}) {:#}", .field_name.join("."), .type_name.join(", "), .error)]
    ErrorInNestedField {
        type_name: Vec<&'static str>,
        field_name: Vec<&'static str>,
        error: String,
    },
    #[error("`{}` is not a valid type to use as a field name in `{}`", .key_type, .type_name)]
    InvalidFieldType {
        type_name: &'static str,
        key_type: String,
    },
    #[error("{}::{} is deprecated: {}", .type_name, .field_name, .reason)]
    DeprecatedField {
        type_name: &'static str,
        field_name: &'static str,
        reason: &'static str,
    },
}

impl Error {
    /// Log a warning; if a warning collector is set for the current thread,
    /// use it, otherwise, log a regular warning message.
    #[cfg(feature = "std")]
    pub fn warn(message: String) {
        WARNING_COLLECTOR.with(|collector| {
            let collector = collector.borrow();
            if let Some(collector) = collector.as_ref() {
                collector.warn(message);
            } else {
                log::warn!("{message}");
            }
        });
    }

    #[cfg(feature = "std")]
    pub fn capture_warnings<F: FnOnce() -> T, T>(f: F) -> (T, Vec<String>) {
        let warnings = Rc::new(RefCell::new(vec![]));

        struct Collector {
            warnings: Rc<RefCell<Vec<String>>>,
        }

        impl WarningCollector for Collector {
            fn warn(&self, message: String) {
                self.warnings.borrow_mut().push(message);
            }
        }

        Self::set_warning_collector(Collector {
            warnings: Rc::clone(&warnings),
        });
        let result = f();
        Self::clear_warning_collector();
        let warnings = match Rc::try_unwrap(warnings) {
            Ok(warnings) => warnings.into_inner(),
            Err(warnings) => (*warnings).clone().into_inner(),
        };
        (result, warnings)
    }

    /// Replace the warning collector for the current thread
    #[cfg(feature = "std")]
    fn set_warning_collector<T: WarningCollector + 'static>(c: T) {
        WARNING_COLLECTOR.with(|collector| {
            collector.borrow_mut().replace(Box::new(c));
        });
    }

    /// Clear the warning collector for the current thread
    #[cfg(feature = "std")]
    fn clear_warning_collector() {
        WARNING_COLLECTOR.with(|collector| {
            collector.borrow_mut().take();
        });
    }

    fn compute_unknown_fields(
        type_name: &'static str,
        object: &crate::Object,
        possible: &'static [&'static str],
    ) -> Vec<Self> {
        let mut errors = Vec::new();

        for key in object.keys() {
            match key {
                Value::String(s) => {
                    if !possible.contains(&s.as_str()) {
                        errors.push(Self::UnknownFieldForStruct {
                            field_name: s.to_string(),
                            type_name,
                            possible,
                        });
                    }
                }
                other => {
                    errors.push(Self::InvalidFieldType {
                        type_name,
                        key_type: other.variant_name().to_string(),
                    });
                }
            }
        }

        errors
    }

    pub fn raise_deprecated_fields(
        options: FromDynamicOptions,
        type_name: &'static str,
        field_name: &'static str,
        reason: &'static str,
    ) -> Result<(), Self> {
        if options.deprecated_fields == UnknownFieldAction::Ignore {
            return Ok(());
        }
        let err = Self::DeprecatedField {
            type_name,
            field_name,
            reason,
        };

        match options.deprecated_fields {
            UnknownFieldAction::Deny => Err(err),
            UnknownFieldAction::Warn => {
                #[cfg(feature = "std")]
                Self::warn(format!("{:#}", err));
                Ok(())
            }
            UnknownFieldAction::Ignore => unreachable!(),
        }
    }

    pub fn raise_unknown_fields(
        options: FromDynamicOptions,
        type_name: &'static str,
        object: &crate::Object,
        possible: &'static [&'static str],
    ) -> Result<(), Self> {
        if options.unknown_fields == UnknownFieldAction::Ignore {
            return Ok(());
        }

        let errors = Self::compute_unknown_fields(type_name, object, possible);
        if errors.is_empty() {
            return Ok(());
        }

        #[cfg(feature = "std")]
        {
            let show_warning =
                options.unknown_fields == UnknownFieldAction::Warn || errors.len() > 1;

            if show_warning {
                for err in &errors {
                    Self::warn(format!("{:#}", err));
                }
            }
        }

        if options.unknown_fields == UnknownFieldAction::Deny {
            if let Some(err) = errors.into_iter().next() {
                return Err(err);
            }
        }

        Ok(())
    }

    #[cfg(not(feature = "std"))]
    fn possible_matches(_used: &str, _possible: &'static [&'static str]) -> &'static str {
        ""
    }

    #[cfg(feature = "std")]
    fn possible_matches(used: &str, possible: &'static [&'static str]) -> String {
        // Produce similar field name list
        let mut candidates: Vec<(f64, &str)> = possible
            .iter()
            .map(|&name| (strsim::jaro_winkler(used, name), name))
            .filter(|(confidence, _)| *confidence > 0.8)
            .collect();
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(core::cmp::Ordering::Equal));
        let suggestions: Vec<&str> = candidates.into_iter().map(|(_, name)| name).collect();

        // Filter the suggestions out of the allowed field names
        // and sort what remains.
        let mut fields: Vec<&str> = possible
            .iter()
            .filter(|&name| !suggestions.iter().any(|candidate| candidate == name))
            .copied()
            .collect();
        fields.sort_unstable();

        let mut message = String::new();

        match suggestions.len() {
            0 => {}
            1 => message.push_str(&format!("Did you mean `{}`?", suggestions[0])),
            _ => {
                message.push_str("Did you mean one of ");
                for (idx, candidate) in suggestions.iter().enumerate() {
                    if idx > 0 {
                        message.push_str(", ");
                    }
                    message.push('`');
                    message.push_str(candidate);
                    message.push('`');
                }
                message.push('?');
            }
        }
        if !fields.is_empty() {
            let limit = 5;
            if fields.len() > limit {
                message.push_str(
                    " There are too many alternatives to list here; consult the documentation!",
                );
            } else {
                if suggestions.is_empty() {
                    message.push_str("Possible alternatives are ");
                } else if suggestions.len() == 1 {
                    message.push_str(" The other option is ");
                } else {
                    message.push_str(" Other alternatives are ");
                }
                for (idx, candidate) in fields.iter().enumerate() {
                    if idx > 0 {
                        message.push_str(", ");
                    }
                    message.push('`');
                    message.push_str(candidate);
                    message.push('`');
                }
            }
        }

        message
    }

    pub fn field_context(
        self,
        type_name: &'static str,
        field_name: &'static str,
        obj: &Object,
    ) -> Self {
        let is_leaf = !matches!(self, Self::ErrorInField { .. });
        fn add_obj_context(is_leaf: bool, obj: &Object, message: String) -> String {
            if is_leaf {
                // Show the object as context.
                // However, some objects, like the main config, are very large and
                // it isn't helpful to show that, so only include it when the context
                // is more reasonable.
                let obj_str = format!("{:#?}", obj);
                if obj_str.len() > 128 || obj_str.lines().count() > 10 {
                    message
                } else {
                    format!("{}.\n{}", message, obj_str)
                }
            } else {
                message
            }
        }

        match self {
            Self::NoConversion { source_type, .. } if source_type == "Null" => Self::ErrorInField {
                type_name,
                field_name,
                error: add_obj_context(is_leaf, obj, format!("missing field `{}`", field_name)),
            },
            Self::ErrorInField {
                type_name: child_type,
                field_name: child_field,
                error,
            } => Self::ErrorInNestedField {
                type_name: vec![type_name, child_type],
                field_name: vec![field_name, child_field],
                error,
            },
            Self::ErrorInNestedField {
                type_name: mut child_type,
                field_name: mut child_field,
                error,
            } => Self::ErrorInNestedField {
                type_name: {
                    child_type.insert(0, type_name);
                    child_type
                },
                field_name: {
                    child_field.insert(0, field_name);
                    child_field
                },
                error,
            },
            _ => Self::ErrorInField {
                type_name,
                field_name,
                error: add_obj_context(is_leaf, obj, format!("{:#}", self)),
            },
        }
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error::Message(s)
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    extern crate std;

    use super::*;
    use alloc::format;
    use alloc::string::ToString;

    // ── Error variant construction and Display ───────────────

    #[test]
    fn message_error_display() {
        let e = Error::Message("something went wrong".to_string());
        let msg = format!("{}", e);
        assert!(msg.contains("something went wrong"));
    }

    #[test]
    fn no_conversion_display() {
        let e = Error::NoConversion {
            source_type: "Bool".to_string(),
            dest_type: "u64",
        };
        let msg = format!("{}", e);
        assert!(msg.contains("Bool"));
        assert!(msg.contains("u64"));
    }

    #[test]
    fn array_size_mismatch_display() {
        let e = Error::ArraySizeMismatch {
            vec_size: 5,
            array_size: 3,
        };
        let msg = format!("{}", e);
        assert!(msg.contains('5'));
        assert!(msg.contains('3'));
    }

    #[test]
    fn char_from_wrong_sized_string_display() {
        let e = Error::CharFromWrongSizedString;
        let msg = format!("{}", e);
        assert!(msg.contains("char"));
        assert!(msg.contains("single character"));
    }

    #[test]
    fn invalid_variant_display() {
        let e = Error::InvalidVariantForType {
            variant_name: "BadVariant".to_string(),
            type_name: "MyEnum",
            possible: &["Good", "Better"],
        };
        let msg = format!("{}", e);
        assert!(msg.contains("BadVariant"));
        assert!(msg.contains("MyEnum"));
    }

    #[test]
    fn unknown_field_display() {
        let e = Error::UnknownFieldForStruct {
            field_name: "badfield".to_string(),
            type_name: "MyStruct",
            possible: &["goodfield", "otherfield"],
        };
        let msg = format!("{}", e);
        assert!(msg.contains("badfield"));
        assert!(msg.contains("MyStruct"));
    }

    #[test]
    fn incorrect_number_of_enum_keys_display() {
        let e = Error::IncorrectNumberOfEnumKeys {
            type_name: "MyEnum",
            num_keys: 3,
        };
        let msg = format!("{}", e);
        assert!(msg.contains("MyEnum"));
        assert!(msg.contains('3'));
    }

    #[test]
    fn error_in_field_display() {
        let e = Error::ErrorInField {
            type_name: "Config",
            field_name: "port",
            error: "invalid value".to_string(),
        };
        let msg = format!("{}", e);
        assert!(msg.contains("Config"));
        assert!(msg.contains("port"));
    }

    #[test]
    fn error_in_nested_field_display() {
        let e = Error::ErrorInNestedField {
            type_name: alloc::vec!["Outer", "Inner"],
            field_name: alloc::vec!["config", "port"],
            error: "out of range".to_string(),
        };
        let msg = format!("{}", e);
        assert!(msg.contains("config"));
        assert!(msg.contains("port"));
    }

    #[test]
    fn invalid_field_type_display() {
        let e = Error::InvalidFieldType {
            type_name: "MyStruct",
            key_type: "U64".to_string(),
        };
        let msg = format!("{}", e);
        assert!(msg.contains("U64"));
        assert!(msg.contains("MyStruct"));
    }

    #[test]
    fn deprecated_field_display() {
        let e = Error::DeprecatedField {
            type_name: "Config",
            field_name: "old_option",
            reason: "use new_option instead",
        };
        let msg = format!("{}", e);
        assert!(msg.contains("deprecated"));
        assert!(msg.contains("old_option"));
        assert!(msg.contains("use new_option instead"));
    }

    // ── From<String> ─────────────────────────────────────────

    #[test]
    fn from_string_creates_message() {
        let e: Error = "custom error".to_string().into();
        let msg = format!("{}", e);
        assert!(msg.contains("custom error"));
    }

    // ── field_context ────────────────────────────────────────

    #[test]
    fn field_context_wraps_no_conversion_null_as_missing_field() {
        let obj = Object::default();
        let err = Error::NoConversion {
            source_type: "Null".to_string(),
            dest_type: "u64",
        };
        let contextualized = err.field_context("Config", "port", &obj);
        match contextualized {
            Error::ErrorInField {
                type_name,
                field_name,
                error,
            } => {
                assert_eq!(type_name, "Config");
                assert_eq!(field_name, "port");
                assert!(error.contains("missing field"));
            }
            _ => panic!("expected ErrorInField"),
        }
    }

    #[test]
    fn field_context_wraps_generic_error() {
        let obj = Object::default();
        let err = Error::Message("bad value".to_string());
        let contextualized = err.field_context("Config", "port", &obj);
        match contextualized {
            Error::ErrorInField {
                type_name,
                field_name,
                ..
            } => {
                assert_eq!(type_name, "Config");
                assert_eq!(field_name, "port");
            }
            _ => panic!("expected ErrorInField"),
        }
    }

    #[test]
    fn field_context_nests_error_in_field() {
        let obj = Object::default();
        let inner = Error::ErrorInField {
            type_name: "Inner",
            field_name: "value",
            error: "invalid".to_string(),
        };
        let nested = inner.field_context("Outer", "config", &obj);
        match nested {
            Error::ErrorInNestedField {
                type_name,
                field_name,
                ..
            } => {
                assert_eq!(type_name, alloc::vec!["Outer", "Inner"]);
                assert_eq!(field_name, alloc::vec!["config", "value"]);
            }
            _ => panic!("expected ErrorInNestedField"),
        }
    }

    #[test]
    fn field_context_extends_nested_field() {
        let obj = Object::default();
        let inner = Error::ErrorInNestedField {
            type_name: alloc::vec!["Mid", "Inner"],
            field_name: alloc::vec!["mid", "inner"],
            error: "deep error".to_string(),
        };
        let deeper = inner.field_context("Outer", "outer", &obj);
        match deeper {
            Error::ErrorInNestedField {
                type_name,
                field_name,
                ..
            } => {
                assert_eq!(type_name, alloc::vec!["Outer", "Mid", "Inner"]);
                assert_eq!(field_name, alloc::vec!["outer", "mid", "inner"]);
            }
            _ => panic!("expected ErrorInNestedField"),
        }
    }

    // ── compute_unknown_fields ───────────────────────────────

    #[test]
    fn raise_unknown_fields_ignore_returns_ok() {
        let opts = FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Ignore,
            ..Default::default()
        };
        let mut obj = Object::default();
        obj.insert(Value::String("unknown".to_string()), Value::Null);
        let result = Error::raise_unknown_fields(opts, "Test", &obj, &["known"]);
        assert!(result.is_ok());
    }

    #[test]
    fn raise_unknown_fields_deny_returns_err() {
        let opts = FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Deny,
            ..Default::default()
        };
        let mut obj = Object::default();
        obj.insert(Value::String("unknown".to_string()), Value::Null);
        let result = Error::raise_unknown_fields(opts, "Test", &obj, &["known"]);
        assert!(result.is_err());
    }

    #[test]
    fn raise_unknown_fields_no_unknowns_is_ok() {
        let opts = FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Deny,
            ..Default::default()
        };
        let mut obj = Object::default();
        obj.insert(Value::String("known".to_string()), Value::Null);
        let result = Error::raise_unknown_fields(opts, "Test", &obj, &["known"]);
        assert!(result.is_ok());
    }

    #[test]
    fn raise_unknown_fields_non_string_key_is_invalid_field_type() {
        let opts = FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Deny,
            ..Default::default()
        };
        let mut obj = Object::default();
        obj.insert(Value::U64(42), Value::Null);
        let result = Error::raise_unknown_fields(opts, "Test", &obj, &[]);
        assert!(result.is_err());
    }

    // ── raise_deprecated_fields ──────────────────────────────

    #[test]
    fn raise_deprecated_ignore_returns_ok() {
        let opts = FromDynamicOptions {
            deprecated_fields: UnknownFieldAction::Ignore,
            ..Default::default()
        };
        let result = Error::raise_deprecated_fields(opts, "Config", "old_field", "use new_field");
        assert!(result.is_ok());
    }

    #[test]
    fn raise_deprecated_deny_returns_err() {
        let opts = FromDynamicOptions {
            deprecated_fields: UnknownFieldAction::Deny,
            ..Default::default()
        };
        let result = Error::raise_deprecated_fields(opts, "Config", "old_field", "use new_field");
        assert!(result.is_err());
    }

    // ── possible_matches (std feature only) ─────────────────

    #[cfg(feature = "std")]
    #[test]
    fn possible_matches_similar_name() {
        let result = Error::possible_matches("colr", &["color", "size", "name"]);
        assert!(result.contains("color"));
    }

    #[cfg(feature = "std")]
    #[test]
    fn possible_matches_no_match() {
        let result = Error::possible_matches("zzzzz", &["alpha", "beta"]);
        // Should list alternatives since no close match
        assert!(result.contains("alpha") || result.contains("beta") || result.is_empty());
    }

    #[cfg(feature = "std")]
    #[test]
    fn possible_matches_exact_match_not_suggested() {
        let result = Error::possible_matches("color", &["color", "size"]);
        // Just ensure no panic
        let _ = result;
    }

    // ── capture_warnings (std feature only) ──────────────────

    #[cfg(feature = "std")]
    #[test]
    fn capture_warnings_collects_warning_messages() {
        let (result, warnings) = Error::capture_warnings(|| {
            Error::warn("test warning 1".to_string());
            Error::warn("test warning 2".to_string());
            42
        });
        assert_eq!(result, 42);
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0], "test warning 1");
        assert_eq!(warnings[1], "test warning 2");
    }

    #[cfg(feature = "std")]
    #[test]
    fn capture_warnings_empty_when_no_warnings() {
        let (result, warnings) = Error::capture_warnings(|| "no warnings");
        assert_eq!(result, "no warnings");
        assert!(warnings.is_empty());
    }
}
