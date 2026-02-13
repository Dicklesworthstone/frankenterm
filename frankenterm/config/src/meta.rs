use frankenterm_dynamic::Value;

/// Trait for returning metadata about config options
pub trait ConfigMeta {
    fn get_config_options(&self) -> &'static [ConfigOption];
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConfigContainer {
    None,
    Option,
    Vec,
    Map,
}

/// Metadata about a config option
pub struct ConfigOption {
    /// The field name
    pub name: &'static str,
    /// Brief documentation
    pub doc: &'static str,
    /// TODO: tags to categorize the option
    pub tags: &'static [&'static str],
    pub container: ConfigContainer,
    /// The type of the field
    pub type_name: &'static str,
    /// call this to get the default value
    pub default_value: Option<fn() -> Value>,
    /// TODO: For enum types, the set of possible values
    pub possible_values: &'static [&'static Value],
    /// TODO: For struct types, the fields in the child struct
    pub fields: &'static [ConfigOption],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_container_none_equality() {
        assert_eq!(ConfigContainer::None, ConfigContainer::None);
    }

    #[test]
    fn config_container_inequality() {
        assert_ne!(ConfigContainer::None, ConfigContainer::Option);
        assert_ne!(ConfigContainer::Option, ConfigContainer::Vec);
        assert_ne!(ConfigContainer::Vec, ConfigContainer::Map);
        assert_ne!(ConfigContainer::Map, ConfigContainer::None);
    }

    #[test]
    fn config_container_clone_copy() {
        let a = ConfigContainer::Vec;
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn config_option_basic() {
        let opt = ConfigOption {
            name: "test_field",
            doc: "A test field",
            tags: &["test"],
            container: ConfigContainer::None,
            type_name: "String",
            default_value: None,
            possible_values: &[],
            fields: &[],
        };
        assert_eq!(opt.name, "test_field");
        assert_eq!(opt.doc, "A test field");
        assert_eq!(opt.container, ConfigContainer::None);
        assert_eq!(opt.type_name, "String");
        assert!(opt.default_value.is_none());
        assert!(opt.possible_values.is_empty());
        assert!(opt.fields.is_empty());
    }
}
