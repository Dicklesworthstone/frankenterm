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
}
