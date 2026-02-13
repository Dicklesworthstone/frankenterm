use crate::config::validate_domain_name;
use frankenterm_dynamic::{FromDynamic, ToDynamic, Value};
#[cfg(feature = "lua")]
use luahelper::impl_lua_conversion_dynamic;

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub enum ValueOrFunc {
    Value(Value),
    Func(String),
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(ValueOrFunc);

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct ExecDomain {
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,
    pub fixup_command: String,
    pub label: Option<ValueOrFunc>,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(ExecDomain);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_or_func_value_variant() {
        let v = ValueOrFunc::Value(Value::Null);
        let dbg = format!("{:?}", v);
        assert!(dbg.contains("Value"));
    }

    #[test]
    fn value_or_func_func_variant() {
        let v = ValueOrFunc::Func("my_func".to_string());
        let dbg = format!("{:?}", v);
        assert!(dbg.contains("Func"));
        assert!(dbg.contains("my_func"));
    }

    #[test]
    fn value_or_func_clone() {
        let v = ValueOrFunc::Func("test".to_string());
        let cloned = v.clone();
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("test"));
    }

    #[test]
    fn exec_domain_debug() {
        let ed = ExecDomain {
            name: "test-domain".to_string(),
            fixup_command: "echo hi".to_string(),
            label: None,
        };
        let dbg = format!("{:?}", ed);
        assert!(dbg.contains("test-domain"));
        assert!(dbg.contains("echo hi"));
    }

    #[test]
    fn exec_domain_clone() {
        let ed = ExecDomain {
            name: "dom".to_string(),
            fixup_command: "cmd".to_string(),
            label: Some(ValueOrFunc::Func("f".to_string())),
        };
        let cloned = ed.clone();
        assert_eq!(cloned.name, "dom");
        assert_eq!(cloned.fixup_command, "cmd");
        assert!(cloned.label.is_some());
    }

    #[test]
    fn exec_domain_no_label() {
        let ed = ExecDomain {
            name: "x".to_string(),
            fixup_command: "y".to_string(),
            label: None,
        };
        assert!(ed.label.is_none());
    }
}
