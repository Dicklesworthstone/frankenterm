use super::*;
use glium::backend::Backend;
use std::ffi::CStr;
use std::io::Error as IoError;
use std::os::raw::c_void;
use std::ptr::{null, null_mut};
use winapi::shared::windef::*;
use winapi::um::libloaderapi::{GetModuleHandleW, *};
use winapi::um::wingdi::*;
use winapi::um::winuser::*;

pub mod ffi {
    include!(concat!(env!("OUT_DIR"), "/wgl_bindings.rs"));
}
pub mod ffiextra {
    include!(concat!(env!("OUT_DIR"), "/wgl_extra_bindings.rs"));
}

struct WglWrapper {
    lib: libloading::Library,
    wgl: ffi::Wgl,
    ext: Option<ffiextra::Wgl>,
}

type GetProcAddressFunc =
    unsafe extern "system" fn(*const std::os::raw::c_char) -> *const std::os::raw::c_void;

impl Drop for WglWrapper {
    fn drop(&mut self) {
        log::trace!("dropping WglWrapper and libloading {:?}", self.lib);
    }
}

impl WglWrapper {
    fn load() -> anyhow::Result<Self> {
        let class_name = wide_string("wezterm wgl extension probing window");
        let h_inst = unsafe { GetModuleHandleW(null()) };
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
            lpfnWndProc: Some(DefWindowProcW),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: h_inst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
        };

        if unsafe { RegisterClassW(&class) } == 0 {
            let err = IoError::last_os_error();
            match err.raw_os_error() {
                Some(code)
                    if code == winapi::shared::winerror::ERROR_CLASS_ALREADY_EXISTS as i32 => {}
                _ => return Err(err.into()),
            }
        }

        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1024,
                768,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if hwnd.is_null() {
            let err = IoError::last_os_error();
            anyhow::bail!("CreateWindowExW: {}", err);
        }

        let mut state = GlState::create_basic(WglWrapper::create()?, hwnd)?;

        state.make_current_checked()?;

        let Some(wgl) = state.wgl.as_mut() else {
            anyhow::bail!("WGL wrapper unavailable while probing extensions");
        };
        let _ = wgl.load_ext();

        state.make_not_current();

        Ok(state.into_wrapper())
    }

    fn create() -> anyhow::Result<Self> {
        if crate::configuration::prefer_swrast() {
            let exe_path = std::env::current_exe()
                .map_err(|err| anyhow::anyhow!("failed to resolve current executable: {err}"))?;
            let exe_dir = exe_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("current executable has no parent directory"))?;
            let mesa_dir = exe_dir.join("mesa");
            let mesa_dir = mesa_dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Mesa DLL directory is not valid UTF-8"))?;
            let mesa_dir = wide_string(mesa_dir);

            unsafe {
                AddDllDirectory(mesa_dir.as_ptr());
                SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
            }
        }

        let lib = unsafe { libloading::Library::new("opengl32.dll") }.map_err(|e| {
            log::error!("{:?}", e);
            e
        })?;
        log::trace!("loaded {:?}", lib);

        let get_proc_address: libloading::Symbol<GetProcAddressFunc> =
            unsafe { lib.get(b"wglGetProcAddress\0")? };
        let wgl = ffi::Wgl::load_with(|s: &'static str| {
            let sym_name = std::ffi::CString::new(s).expect("symbol to be cstring compatible");
            if let Ok(sym) = unsafe { lib.get(sym_name.as_bytes_with_nul()) } {
                return *sym;
            }
            unsafe { get_proc_address(sym_name.as_ptr()) }
        });
        Ok(Self {
            lib,
            wgl,
            ext: None,
        })
    }

    fn load_ext(&mut self) -> anyhow::Result<()> {
        let get_proc_address: libloading::Symbol<GetProcAddressFunc> =
            unsafe { self.lib.get(b"wglGetProcAddress\0")? };

        self.ext
            .replace(ffiextra::Wgl::load_with(|s: &'static str| {
                let sym_name = std::ffi::CString::new(s).expect("symbol to be cstring compatible");
                if let Ok(sym) = unsafe { self.lib.get(sym_name.as_bytes_with_nul()) } {
                    return *sym;
                }
                unsafe { get_proc_address(sym_name.as_ptr()) }
            }));

        Ok(())
    }
}

pub struct GlState {
    wgl: Option<WglWrapper>,
    hdc: HDC,
    rc: ffi::types::HGLRC,
}

fn has_extension(extensions: &str, wanted: &str) -> bool {
    extensions.split(' ').find(|&ext| ext == wanted).is_some()
}

fn client_rect_dimensions(rect: RECT) -> (u32, u32) {
    (
        rect.right.saturating_sub(rect.left) as u32,
        rect.bottom.saturating_sub(rect.top) as u32,
    )
}

fn current_client_rect(hdc: HDC) -> Option<RECT> {
    let hwnd = unsafe { WindowFromDC(hdc) };
    if hwnd.is_null() {
        log::warn!("WindowFromDC returned null while querying WGL framebuffer size");
        return None;
    }

    let mut rect: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        log::warn!(
            "GetClientRect failed while querying WGL framebuffer size: {}",
            std::io::Error::last_os_error()
        );
        None
    } else {
        Some(rect)
    }
}

fn get_window_dc(window: HWND, operation: &str) -> anyhow::Result<HDC> {
    let hdc = unsafe { GetDC(window) };
    if hdc.is_null() {
        anyhow::bail!(
            "{operation}: GetDC returned null: {}",
            IoError::last_os_error()
        );
    }
    Ok(hdc)
}

fn wgl_extension_string(data: *const i8, operation: &str) -> String {
    if data.is_null() {
        log::warn!("{operation} returned a null WGL extension string");
        String::new()
    } else {
        unsafe { CStr::from_ptr(data) }
            .to_string_lossy()
            .into_owned()
    }
}

impl GlState {
    fn into_wrapper(mut self) -> WglWrapper {
        self.delete();
        self.wgl.take().unwrap()
    }

    pub fn create(window: HWND) -> anyhow::Result<Self> {
        let wgl = WglWrapper::load()?;

        if let Some(ext) = wgl.ext.as_ref() {
            let hdc = get_window_dc(window, "querying WGL extensions")?;

            let extensions = if ext.GetExtensionsStringARB.is_loaded() {
                let data = unsafe { ext.GetExtensionsStringARB(hdc as *const _) };
                wgl_extension_string(data, "wglGetExtensionsStringARB")
            } else if ext.GetExtensionsStringEXT.is_loaded() {
                let data = unsafe { ext.GetExtensionsStringEXT() };
                wgl_extension_string(data, "wglGetExtensionsStringEXT")
            } else {
                "".to_owned()
            };
            log::trace!("opengl extensions: {:?}", extensions);

            if has_extension(&extensions, "WGL_ARB_pixel_format") {
                return match Self::create_ext(wgl, extensions, hdc) {
                    Ok(state) => Ok(state),
                    Err(err) => {
                        log::warn!(
                            "failed to created extended OpenGL context \
                            ({}), fall back to basic",
                            err
                        );
                        let wgl = WglWrapper::load()?;
                        Self::create_basic(wgl, window)
                    }
                };
            }
        }

        Self::create_basic(wgl, window)
    }

    fn create_ext(wgl: WglWrapper, extensions: String, hdc: HDC) -> anyhow::Result<Self> {
        use ffiextra::*;
        let ext = wgl
            .ext
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("WGL extension table unavailable"))?;

        let mut attribs: Vec<i32> = vec![
            DRAW_TO_WINDOW_ARB as i32,
            1,
            SUPPORT_OPENGL_ARB as i32,
            1,
            DOUBLE_BUFFER_ARB as i32,
            1,
            PIXEL_TYPE_ARB as i32,
            TYPE_RGBA_ARB as i32,
            COLOR_BITS_ARB as i32,
            24,
            ALPHA_BITS_ARB as i32,
            8,
            DEPTH_BITS_ARB as i32,
            24,
            STENCIL_BITS_ARB as i32,
            8,
            SAMPLE_BUFFERS_ARB as i32,
            1,
            SAMPLES_ARB as i32,
            4,
        ];

        if has_extension(&extensions, "WGL_ARB_framebuffer_sRGB") {
            log::trace!("will request FRAMEBUFFER_SRGB_CAPABLE_ARB");
            attribs.push(FRAMEBUFFER_SRGB_CAPABLE_ARB as i32);
            attribs.push(1);
        } else if has_extension(&extensions, "WGL_EXT_framebuffer_sRGB") {
            log::trace!("will request FRAMEBUFFER_SRGB_CAPABLE_EXT");
            attribs.push(FRAMEBUFFER_SRGB_CAPABLE_EXT as i32);
            attribs.push(1);
        }

        attribs.push(0);

        let mut format_id = 0;
        let mut num_formats = 0;

        let res = unsafe {
            ext.ChoosePixelFormatARB(
                hdc as _,
                attribs.as_ptr(),
                null(),
                1,
                &mut format_id,
                &mut num_formats,
            )
        };
        if res == 0 {
            anyhow::bail!("ChoosePixelFormatARB returned 0");
        }

        if num_formats == 0 {
            anyhow::bail!("ChoosePixelFormatARB returned 0 formats");
        }

        let mut pfd: PIXELFORMATDESCRIPTOR = unsafe { std::mem::zeroed() };

        let res = unsafe {
            DescribePixelFormat(
                hdc,
                format_id,
                std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as _,
                &mut pfd,
            )
        };
        if res == 0 {
            anyhow::bail!(
                "DescribePixelFormat function failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let res = unsafe { SetPixelFormat(hdc, format_id, &pfd) };
        if res == 0 {
            anyhow::bail!(
                "SetPixelFormat function failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let mut attribs = vec![
            CONTEXT_MAJOR_VERSION_ARB as i32,
            4,
            CONTEXT_MINOR_VERSION_ARB as i32,
            5,
            CONTEXT_PROFILE_MASK_ARB as i32,
            CONTEXT_CORE_PROFILE_BIT_ARB as i32,
        ];

        if has_extension(&extensions, "WGL_ARB_create_context_robustness") {
            log::trace!("requesting robustness features");
            attribs.push(CONTEXT_RESET_NOTIFICATION_STRATEGY_ARB as i32);
            attribs.push(LOSE_CONTEXT_ON_RESET_ARB as i32);
            attribs.push(CONTEXT_FLAGS_ARB as i32);
            attribs.push(CONTEXT_ROBUST_ACCESS_BIT_ARB as i32);
        }
        attribs.push(0);

        let rc = unsafe { ext.CreateContextAttribsARB(hdc as _, null(), attribs.as_ptr()) };

        if rc.is_null() {
            let err = unsafe { winapi::um::errhandlingapi::GetLastError() };
            anyhow::bail!(
                "CreateContextAttribsARB failed, GetLastError={} {:x}",
                err,
                err
            );
        }

        let state = Self {
            wgl: Some(wgl),
            rc,
            hdc,
        };
        state.make_current_checked()?;
        Ok(state)
    }

    fn create_basic(wgl: WglWrapper, window: HWND) -> anyhow::Result<Self> {
        let hdc = get_window_dc(window, "creating basic WGL context")?;

        let pfd = PIXELFORMATDESCRIPTOR {
            nSize: std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as u16,
            nVersion: 1,
            dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
            iPixelType: PFD_TYPE_RGBA,
            cColorBits: 24,
            cRedBits: 0,
            cRedShift: 0,
            cGreenBits: 0,
            cGreenShift: 0,
            cBlueBits: 0,
            cBlueShift: 0,
            cAlphaBits: 8,
            cAlphaShift: 0,
            cAccumBits: 0,
            cAccumRedBits: 0,
            cAccumGreenBits: 0,
            cAccumBlueBits: 0,
            cAccumAlphaBits: 0,
            cDepthBits: 24,
            cStencilBits: 8,
            cAuxBuffers: 0,
            iLayerType: PFD_MAIN_PLANE,
            bReserved: 0,
            dwLayerMask: 0,
            dwVisibleMask: 0,
            dwDamageMask: 0,
        };
        let format = unsafe { ChoosePixelFormat(hdc, &pfd) };
        if format == 0 {
            anyhow::bail!(
                "ChoosePixelFormat failed while creating basic WGL context: {}",
                IoError::last_os_error()
            );
        }

        let res = unsafe { SetPixelFormat(hdc, format, &pfd) };
        if res == 0 {
            anyhow::bail!(
                "SetPixelFormat failed while creating basic WGL context: {}",
                IoError::last_os_error()
            );
        }

        let rc = unsafe { wgl.wgl.CreateContext(hdc as *mut _) };
        if rc.is_null() {
            anyhow::bail!(
                "CreateContext failed while creating basic WGL context: {}",
                IoError::last_os_error()
            );
        }

        let state = Self {
            wgl: Some(wgl),
            rc,
            hdc,
        };
        state.make_current_checked()?;
        Ok(state)
    }

    fn make_not_current(&self) {
        if let Some(wgl) = self.wgl.as_ref() {
            unsafe {
                wgl.wgl.MakeCurrent(self.hdc as *mut _, std::ptr::null());
            }
        }
    }

    fn delete(&mut self) {
        self.make_not_current();
        if let Some(wgl) = self.wgl.as_ref() {
            unsafe {
                wgl.wgl.DeleteContext(self.rc);
            }
        }
    }

    fn make_current_checked(&self) -> anyhow::Result<()> {
        let Some(wgl) = self.wgl.as_ref() else {
            anyhow::bail!("WGL wrapper unavailable while making context current");
        };
        let res = unsafe { wgl.wgl.MakeCurrent(self.hdc as *mut _, self.rc) };
        if res == 0 {
            anyhow::bail!("wglMakeCurrent failed: {}", IoError::last_os_error());
        }
        Ok(())
    }
}

impl Drop for GlState {
    fn drop(&mut self) {
        self.delete();
    }
}

unsafe impl glium::backend::Backend for GlState {
    fn resize(&self, new_size: (u32, u32)) {
        if let Some(actual) = current_client_rect(self.hdc).map(client_rect_dimensions) {
            if actual != new_size {
                log::trace!(
                    "WGL drawable size is window-managed; requested {:?}, current client rect {:?}",
                    new_size,
                    actual
                );
            }
        }
    }

    fn swap_buffers(&self) -> Result<(), glium::SwapBuffersError> {
        unsafe {
            SwapBuffers(self.hdc);
        }
        Ok(())
    }

    unsafe fn get_proc_address(&self, symbol: &str) -> *const c_void {
        let sym_name = std::ffi::CString::new(symbol).expect("symbol to be cstring compatible");
        let Some(wgl) = self.wgl.as_ref() else {
            log::error!("WGL wrapper unavailable while resolving {symbol}");
            return null();
        };
        if let Ok(sym) = wgl.lib.get(sym_name.as_bytes_with_nul()) {
            //eprintln!("{} -> {:?}", symbol, sym);
            return *sym;
        }
        let res = wgl.wgl.GetProcAddress(sym_name.as_ptr()) as *const c_void;
        // eprintln!("{} -> {:?}", symbol, res);
        res
    }

    fn get_framebuffer_dimensions(&self) -> (u32, u32) {
        current_client_rect(self.hdc)
            .map(client_rect_dimensions)
            .unwrap_or((0, 0))
    }

    fn is_current(&self) -> bool {
        self.wgl
            .as_ref()
            .map(|wgl| unsafe { wgl.wgl.GetCurrentContext() == self.rc })
            .unwrap_or(false)
    }

    unsafe fn make_current(&self) {
        if let Err(err) = self.make_current_checked() {
            log::error!("unable to make WGL context current: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{client_rect_dimensions, wgl_extension_string};
    use std::ffi::CString;
    use std::ptr::null;
    use winapi::shared::windef::RECT;

    #[test]
    fn client_rect_dimensions_handles_positive_rects() {
        assert_eq!(
            client_rect_dimensions(RECT {
                left: 4,
                top: 8,
                right: 104,
                bottom: 48,
            }),
            (100, 40)
        );
    }

    #[test]
    fn client_rect_dimensions_saturates_inverted_rects() {
        assert_eq!(
            client_rect_dimensions(RECT {
                left: 10,
                top: 10,
                right: 3,
                bottom: 1,
            }),
            (0, 0)
        );
    }

    #[test]
    fn wgl_extension_string_handles_null_pointer() {
        assert_eq!(wgl_extension_string(null(), "test extension query"), "");
    }

    #[test]
    fn wgl_extension_string_reads_valid_c_string() {
        let extensions = CString::new("WGL_ARB_pixel_format WGL_EXT_framebuffer_sRGB").unwrap();
        assert_eq!(
            wgl_extension_string(extensions.as_ptr(), "test extension query"),
            "WGL_ARB_pixel_format WGL_EXT_framebuffer_sRGB"
        );
    }
}
