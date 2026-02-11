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
