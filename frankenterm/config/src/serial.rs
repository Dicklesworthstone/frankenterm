use crate::config::validate_domain_name;
use frankenterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Default, Debug, Clone, FromDynamic, ToDynamic)]
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
    pub baud: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
