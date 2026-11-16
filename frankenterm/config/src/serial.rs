use crate::config::validate_domain_name;
use frankenterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Default, Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub struct SerialDomain {
    /// The name of this specific domain.  Must be unique amongst
    /// all types of domain in the configuration file.
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    /// Specifies the serial device name.
    /// On Windows systems this can be a name like `COM0`.
    /// On posix systems this will be something like `/dev/ttyUSB0`.
    /// If omitted, the name will be interpreted as the port.
    pub port: Option<String>,

    /// Set the baud rate.  The default is 9600 baud.
    pub baud: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_dynamic::Value;
    use std::collections::BTreeMap;

    #[test]
    fn serial_domain_default() {
        let sd = SerialDomain::default();
        assert_eq!(sd.name, "");
        assert!(sd.port.is_none());
        assert!(sd.baud.is_none());
    }

    #[test]
    fn serial_domain_debug() {
        let sd = SerialDomain::default();
        let dbg = format!("{:?}", sd);
        assert!(dbg.contains("SerialDomain"));
    }

    #[test]
    fn serial_domain_clone() {
        let sd = SerialDomain {
            name: "ttyUSB0".to_string(),
            port: Some("/dev/ttyUSB0".to_string()),
            baud: Some(115200),
        };
        let cloned = sd.clone();
        assert_eq!(cloned.name, "ttyUSB0");
        assert_eq!(cloned.port.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(cloned.baud, Some(115200));
    }

    #[test]
    fn serial_domain_with_custom_baud() {
        let sd = SerialDomain {
            name: "com0".to_string(),
            port: None,
            baud: Some(9600),
        };
        assert_eq!(sd.baud, Some(9600));
        assert!(sd.port.is_none());
    }

    #[test]
    fn serial_domain_rejects_baud_above_backend_boundary_during_config_decode() {
        let serial_config = |baud| {
            Value::Object(
                BTreeMap::from([
                    (
                        Value::String("name".to_string()),
                        Value::String("bounded-serial".to_string()),
                    ),
                    (Value::String("baud".to_string()), Value::U64(baud)),
                ])
                .into(),
            )
        };

        let maximum =
            SerialDomain::from_dynamic(&serial_config(u64::from(u32::MAX)), Default::default())
                .expect("the serial backend's maximum representable baud must decode");
        let exact_backend_type: Option<u32> = maximum.baud;
        assert_eq!(exact_backend_type, Some(u32::MAX));

        let first_unrepresentable = u64::from(u32::MAX) + 1;
        let rejected =
            SerialDomain::from_dynamic(&serial_config(first_unrepresentable), Default::default());
        assert!(
            rejected.is_err(),
            "baud {} must fail at config decode, before domain construction",
            first_unrepresentable
        );
    }
}
