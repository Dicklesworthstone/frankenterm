use frankenterm_dynamic::{FromDynamic, ToDynamic};
#[cfg(feature = "lua")]
use luahelper::impl_lua_conversion_dynamic;

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic, Default)]
pub enum FrontEndSelection {
    #[default]
    OpenGL,
    WebGpu,
    Software,
}

/// Corresponds to <https://docs.rs/wgpu/latest/wgpu/struct.AdapterInfo.html>
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct GpuInfo {
    pub name: String,
    pub device_type: String,
    pub backend: String,
    pub driver: Option<String>,
    pub driver_info: Option<String>,
    pub vendor: Option<u32>,
    pub device: Option<u32>,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(GpuInfo);

impl ToString for GpuInfo {
    fn to_string(&self) -> String {
        let mut result = format!(
            "name={}, device_type={}, backend={}",
            self.name, self.device_type, self.backend
        );
        if let Some(driver) = &self.driver {
            result.push_str(&format!(", driver={driver}"));
        }
        if let Some(driver_info) = &self.driver_info {
            result.push_str(&format!(", driver_info={driver_info}"));
        }
        if let Some(vendor) = &self.vendor {
            result.push_str(&format!(", vendor={vendor}"));
        }
        if let Some(device) = &self.device {
            result.push_str(&format!(", device={device}"));
        }
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum WebGpuPowerPreference {
    LowPower,
    HighPerformance,
}

impl Default for WebGpuPowerPreference {
    fn default() -> Self {
        Self::LowPower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_selection_default_is_opengl() {
        assert_eq!(FrontEndSelection::default(), FrontEndSelection::OpenGL);
    }

    #[test]
    fn webgpu_power_preference_default_is_low_power() {
        assert_eq!(
            WebGpuPowerPreference::default(),
            WebGpuPowerPreference::LowPower
        );
    }

    #[test]
    fn gpu_info_to_string_includes_optional_fields_when_present() {
        let info = GpuInfo {
            name: "MyGPU".to_string(),
            device_type: "discrete".to_string(),
            backend: "vulkan".to_string(),
            driver: Some("driver-x".to_string()),
            driver_info: Some("driver-info".to_string()),
            vendor: Some(1234),
            device: Some(5678),
        };

        let s = info.to_string();
        assert!(s.contains("name=MyGPU"));
        assert!(s.contains("device_type=discrete"));
        assert!(s.contains("backend=vulkan"));
        assert!(s.contains("driver=driver-x"));
        assert!(s.contains("driver_info=driver-info"));
        assert!(s.contains("vendor=1234"));
        assert!(s.contains("device=5678"));
    }

    #[test]
    fn gpu_info_to_string_omits_optional_fields_when_absent() {
        let info = GpuInfo {
            name: "MyGPU".to_string(),
            device_type: "integrated".to_string(),
            backend: "metal".to_string(),
            driver: None,
            driver_info: None,
            vendor: None,
            device: None,
        };

        let s = info.to_string();
        assert!(s.contains("name=MyGPU"));
        assert!(!s.contains("driver="));
        assert!(!s.contains("driver_info="));
        assert!(!s.contains("vendor="));
        assert!(!s.contains("device="));
    }

    #[test]
    fn frontend_selection_variants_differ() {
        assert_ne!(FrontEndSelection::OpenGL, FrontEndSelection::WebGpu);
        assert_ne!(FrontEndSelection::OpenGL, FrontEndSelection::Software);
        assert_ne!(FrontEndSelection::WebGpu, FrontEndSelection::Software);
    }

    #[test]
    fn frontend_selection_clone_copy() {
        let a = FrontEndSelection::WebGpu;
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn frontend_selection_debug() {
        let dbg = format!("{:?}", FrontEndSelection::OpenGL);
        assert!(dbg.contains("OpenGL"));
        let dbg = format!("{:?}", FrontEndSelection::Software);
        assert!(dbg.contains("Software"));
    }

    #[test]
    fn webgpu_power_preference_variants_differ() {
        assert_ne!(
            WebGpuPowerPreference::LowPower,
            WebGpuPowerPreference::HighPerformance
        );
    }

    #[test]
    fn webgpu_power_preference_clone_copy() {
        let a = WebGpuPowerPreference::HighPerformance;
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn webgpu_power_preference_debug() {
        let dbg = format!("{:?}", WebGpuPowerPreference::LowPower);
        assert!(dbg.contains("LowPower"));
        let dbg = format!("{:?}", WebGpuPowerPreference::HighPerformance);
        assert!(dbg.contains("HighPerformance"));
    }

    #[test]
    fn gpu_info_to_string_required_fields_only() {
        let info = GpuInfo {
            name: "A".to_string(),
            device_type: "B".to_string(),
            backend: "C".to_string(),
            driver: None,
            driver_info: None,
            vendor: None,
            device: None,
        };
        let s = info.to_string();
        assert_eq!(s, "name=A, device_type=B, backend=C");
    }

    #[test]
    fn gpu_info_debug_includes_name() {
        let info = GpuInfo {
            name: "TestGPU".to_string(),
            device_type: "discrete".to_string(),
            backend: "vulkan".to_string(),
            driver: None,
            driver_info: None,
            vendor: None,
            device: None,
        };
        let dbg = format!("{:?}", info);
        assert!(dbg.contains("TestGPU"));
    }

    #[test]
    fn gpu_info_clone() {
        let info = GpuInfo {
            name: "GPU".to_string(),
            device_type: "integrated".to_string(),
            backend: "metal".to_string(),
            driver: Some("drv".to_string()),
            driver_info: None,
            vendor: Some(42),
            device: None,
        };
        let cloned = info.clone();
        assert_eq!(cloned.name, "GPU");
        assert_eq!(cloned.vendor, Some(42));
    }

    #[test]
    fn gpu_info_to_string_partial_optionals() {
        let info = GpuInfo {
            name: "X".to_string(),
            device_type: "Y".to_string(),
            backend: "Z".to_string(),
            driver: Some("d".to_string()),
            driver_info: None,
            vendor: None,
            device: Some(99),
        };
        let s = info.to_string();
        assert!(s.contains("driver=d"));
        assert!(!s.contains("driver_info="));
        assert!(!s.contains("vendor="));
        assert!(s.contains("device=99"));
    }
}
