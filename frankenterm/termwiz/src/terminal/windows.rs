use crate::escape::csi::{DecPrivateMode, DecPrivateModeCode, Mode, CSI};
use crate::istty::IsTty;
use crate::terminal::ProbeCapabilities;
use crate::{bail, ensure, format_err, Result};
use filedescriptor::{FileDescriptor, OwnedHandle};
use std::cmp::{max, min};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{stdin, stdout, Error as IoError, Read, Result as IoResult, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;
use std::time::Duration;
use std::{mem, ptr};
use winapi::shared::winerror::WAIT_TIMEOUT;
use winapi::um::consoleapi;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::synchapi::{CreateEventW, SetEvent, WaitForMultipleObjects};
use winapi::um::winbase::{INFINITE, WAIT_FAILED, WAIT_OBJECT_0};
use winapi::um::wincon::{
    CreateConsoleScreenBuffer, FillConsoleOutputAttribute, FillConsoleOutputCharacterW,
    GetConsoleScreenBufferInfo, ReadConsoleOutputW, ScrollConsoleScreenBufferW,
    SetConsoleActiveScreenBuffer, SetConsoleCP, SetConsoleCursorPosition, SetConsoleOutputCP,
    SetConsoleScreenBufferSize, SetConsoleTextAttribute, SetConsoleWindowInfo, WriteConsoleOutputW,
    CHAR_INFO, CONSOLE_SCREEN_BUFFER_INFO, CONSOLE_TEXTMODE_BUFFER, COORD,
    DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT,
    ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    ENABLE_WINDOW_INPUT, INPUT_RECORD, SMALL_RECT,
};
use winapi::um::winnls::CP_UTF8;
use winapi::um::winnt::{FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE};

use crate::caps::Capabilities;
use crate::input::{InputEvent, InputParser};
use crate::render::terminfo::TerminfoRenderer;
use crate::render::windows::WindowsConsoleRenderer;
use crate::render::RenderTty;
use crate::surface::Change;
use crate::terminal::{cast, ScreenSize, Terminal};

const BUF_SIZE: usize = 128;

enum Renderer {
    Terminfo(TerminfoRenderer),
    Windows(WindowsConsoleRenderer),
}

pub trait ConsoleInputHandle {
    fn set_input_mode(&mut self, mode: u32) -> Result<()>;
    fn get_input_mode(&mut self) -> Result<u32>;
    fn set_input_cp(&mut self, cp: u32) -> Result<()>;
    fn get_input_cp(&mut self) -> u32;
    fn get_number_of_input_events(&mut self) -> Result<usize>;
    fn read_console_input(&mut self, num_events: usize) -> Result<Vec<INPUT_RECORD>>;
}

pub trait ConsoleOutputHandle {
    fn set_output_mode(&mut self, mode: u32) -> Result<()>;
    fn get_output_mode(&mut self) -> Result<u32>;
    fn set_output_cp(&mut self, cp: u32) -> Result<()>;
    fn get_output_cp(&mut self) -> u32;
    fn fill_char(&mut self, text: char, x: i16, y: i16, len: u32) -> Result<u32>;
    fn fill_attr(&mut self, attr: u16, x: i16, y: i16, len: u32) -> Result<u32>;
    fn set_attr(&mut self, attr: u16) -> Result<()>;
    fn set_cursor_position(&mut self, x: i16, y: i16) -> Result<()>;
    fn get_buffer_info(&mut self) -> Result<CONSOLE_SCREEN_BUFFER_INFO>;
    fn get_buffer_contents(&mut self) -> Result<Vec<CHAR_INFO>>;
    fn set_buffer_contents(&mut self, buffer: &[CHAR_INFO]) -> Result<()>;
    fn set_viewport(&mut self, left: i16, top: i16, right: i16, bottom: i16) -> Result<()>;
    fn scroll_region(
        &mut self,
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
        dx: i16,
        dy: i16,
        attr: u16,
    ) -> Result<()>;
}

struct InputHandle {
    handle: FileDescriptor,
}

impl Read for InputHandle {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        self.handle.read(buf)
    }
}

impl ConsoleInputHandle for InputHandle {
    fn set_input_mode(&mut self, mode: u32) -> Result<()> {
        if unsafe { consoleapi::SetConsoleMode(self.handle.as_raw_handle() as *mut _, mode) } == 0 {
            bail!("SetConsoleMode failed: {}", IoError::last_os_error());
        }
        Ok(())
    }

    fn get_input_mode(&mut self) -> Result<u32> {
        let mut mode = 0;
        if unsafe { consoleapi::GetConsoleMode(self.handle.as_raw_handle() as *mut _, &mut mode) }
            == 0
        {
            bail!("GetConsoleMode failed: {}", IoError::last_os_error());
        }
        Ok(mode)
    }

    fn set_input_cp(&mut self, cp: u32) -> Result<()> {
        if unsafe { SetConsoleCP(cp) } == 0 {
            bail!("SetConsoleCP failed: {}", IoError::last_os_error());
        }
        Ok(())
    }

    fn get_input_cp(&mut self) -> u32 {
        unsafe { consoleapi::GetConsoleCP() }
    }

    fn get_number_of_input_events(&mut self) -> Result<usize> {
        let mut num = 0;
        if unsafe {
            consoleapi::GetNumberOfConsoleInputEvents(
                self.handle.as_raw_handle() as *mut _,
                &mut num,
            )
        } == 0
        {
            bail!(
                "GetNumberOfConsoleInputEvents failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(num as usize)
    }

    fn read_console_input(&mut self, num_events: usize) -> Result<Vec<INPUT_RECORD>> {
        let mut res = Vec::with_capacity(num_events);
        let empty_record: INPUT_RECORD = unsafe { mem::zeroed() };
        res.resize(num_events, empty_record);

        let mut num = 0;

        if unsafe {
            consoleapi::ReadConsoleInputW(
                self.handle.as_raw_handle() as *mut _,
                res.as_mut_ptr(),
                num_events as u32,
                &mut num,
            )
        } == 0
        {
            bail!("ReadConsoleInput failed: {}", IoError::last_os_error());
        }

        unsafe { res.set_len(num as usize) };
        Ok(res)
    }
}

struct OutputHandle {
    handle: FileDescriptor,
    write_buffer: Vec<u8>,
}

impl OutputHandle {
    fn new(handle: FileDescriptor) -> Self {
        Self {
            handle,
            write_buffer: Vec::with_capacity(BUF_SIZE),
        }
    }

    fn new_screen_buffer() -> Result<Self> {
        let handle = unsafe {
            CreateConsoleScreenBuffer(
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                CONSOLE_TEXTMODE_BUFFER,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            bail!(
                "CreateConsoleScreenBuffer failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(Self::new(unsafe {
            FileDescriptor::from_raw_handle(handle as _)
        }))
    }

    fn set_active_screen_buffer(&mut self) -> Result<()> {
        self.flush()?;
        if unsafe { SetConsoleActiveScreenBuffer(self.handle.as_raw_handle() as *mut _) } == 0 {
            bail!(
                "SetConsoleActiveScreenBuffer failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(())
    }

    fn set_screen_buffer_size(&mut self, size: COORD) -> Result<()> {
        if unsafe { SetConsoleScreenBufferSize(self.handle.as_raw_handle() as *mut _, size) } != 1 {
            bail!(
                "SetConsoleScreenBufferSize failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(())
    }
}

fn dimensions_from_buffer_info(info: CONSOLE_SCREEN_BUFFER_INFO) -> (usize, usize) {
    let cols = 1 + (info.srWindow.Right - info.srWindow.Left);
    let rows = 1 + (info.srWindow.Bottom - info.srWindow.Top);
    (cols as usize, rows as usize)
}

fn alternate_screen_buffer_geometry(info: CONSOLE_SCREEN_BUFFER_INFO) -> (COORD, SMALL_RECT) {
    let visible_cols = 1 + (info.srWindow.Right - info.srWindow.Left);
    let visible_rows = 1 + (info.srWindow.Bottom - info.srWindow.Top);
    let cols = max(info.dwSize.X, visible_cols).max(1);
    let rows = max(info.dwSize.Y, visible_rows).max(1);
    (
        COORD { X: cols, Y: rows },
        SMALL_RECT {
            Left: 0,
            Top: 0,
            Right: visible_cols.saturating_sub(1),
            Bottom: visible_rows.saturating_sub(1),
        },
    )
}

struct ScreenResizePlan {
    pre_buffer_viewport: Option<SMALL_RECT>,
    buffer_size: COORD,
    final_viewport: SMALL_RECT,
}

fn visible_window_dimensions(info: CONSOLE_SCREEN_BUFFER_INFO) -> COORD {
    COORD {
        X: (1 + info.srWindow.Right - info.srWindow.Left).max(1),
        Y: (1 + info.srWindow.Bottom - info.srWindow.Top).max(1),
    }
}

fn viewport_for_buffer_size(size: COORD) -> SMALL_RECT {
    debug_assert!(size.X > 0);
    debug_assert!(size.Y > 0);
    SMALL_RECT {
        Left: 0,
        Top: 0,
        Right: size.X - 1,
        Bottom: size.Y - 1,
    }
}

fn pre_buffer_resize_viewport(
    info: CONSOLE_SCREEN_BUFFER_INFO,
    target: COORD,
) -> Option<SMALL_RECT> {
    let visible = visible_window_dimensions(info);
    if visible.X <= target.X && visible.Y <= target.Y {
        return None;
    }

    let cols = min(visible.X, target.X).max(1);
    let rows = min(visible.Y, target.Y).max(1);
    let max_left = target.X - cols;
    let max_top = target.Y - rows;
    let left = min(max(info.srWindow.Left, 0), max_left);
    let top = min(max(info.srWindow.Top, 0), max_top);

    Some(SMALL_RECT {
        Left: left,
        Top: top,
        Right: left + cols - 1,
        Bottom: top + rows - 1,
    })
}

fn screen_resize_plan(info: CONSOLE_SCREEN_BUFFER_INFO, target: COORD) -> ScreenResizePlan {
    ScreenResizePlan {
        pre_buffer_viewport: pre_buffer_resize_viewport(info, target),
        buffer_size: target,
        final_viewport: viewport_for_buffer_size(target),
    }
}

impl RenderTty for OutputHandle {
    fn get_size_in_cells(&mut self) -> Result<(usize, usize)> {
        let info = self.get_buffer_info()?;
        let (cols, rows) = dimensions_from_buffer_info(info);

        Ok((cols, rows))
    }
}

struct EventHandle {
    handle: OwnedHandle,
}

impl EventHandle {
    fn new() -> IoResult<Self> {
        let handle = unsafe { CreateEventW(ptr::null_mut(), 0, 0, ptr::null_mut()) };
        if handle.is_null() {
            Err(IoError::last_os_error())
        } else {
            Ok(Self {
                handle: unsafe { OwnedHandle::from_raw_handle(handle as *mut _) },
            })
        }
    }

    fn set(&self) -> IoResult<()> {
        let ok = unsafe { SetEvent(self.handle.as_raw_handle() as *mut _) };
        if ok == 0 {
            Err(IoError::last_os_error())
        } else {
            Ok(())
        }
    }
}

// Handle created by `CreateEventW` is safe to be shared.
unsafe impl Sync for EventHandle {}

impl Write for OutputHandle {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        if self.write_buffer.len() + buf.len() > self.write_buffer.capacity() {
            self.flush()?;
        }
        if buf.len() >= self.write_buffer.capacity() {
            self.handle.write(buf)
        } else {
            self.write_buffer.write(buf)
        }
    }

    fn flush(&mut self) -> IoResult<()> {
        if !self.write_buffer.is_empty() {
            self.handle.write_all(&self.write_buffer)?;
            self.write_buffer.clear();
        }
        Ok(())
    }
}

impl ConsoleOutputHandle for OutputHandle {
    fn set_output_mode(&mut self, mode: u32) -> Result<()> {
        if unsafe { consoleapi::SetConsoleMode(self.handle.as_raw_handle() as *mut _, mode) } == 0 {
            bail!("SetConsoleMode failed: {}", IoError::last_os_error());
        }
        Ok(())
    }

    fn get_output_mode(&mut self) -> Result<u32> {
        let mut mode = 0;
        if unsafe { consoleapi::GetConsoleMode(self.handle.as_raw_handle() as *mut _, &mut mode) }
            == 0
        {
            bail!("GetConsoleMode failed: {}", IoError::last_os_error());
        }
        Ok(mode)
    }

    fn set_output_cp(&mut self, cp: u32) -> Result<()> {
        if unsafe { SetConsoleOutputCP(cp) } == 0 {
            bail!("SetConsoleOutputCP failed: {}", IoError::last_os_error());
        }
        Ok(())
    }

    fn get_output_cp(&mut self) -> u32 {
        unsafe { consoleapi::GetConsoleOutputCP() }
    }

    fn fill_char(&mut self, text: char, x: i16, y: i16, len: u32) -> Result<u32> {
        let mut wrote = 0;
        if unsafe {
            FillConsoleOutputCharacterW(
                self.handle.as_raw_handle() as *mut _,
                text as u16,
                len,
                COORD { X: x, Y: y },
                &mut wrote,
            )
        } == 0
        {
            bail!(
                "FillConsoleOutputCharacterW failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(wrote)
    }

    fn fill_attr(&mut self, attr: u16, x: i16, y: i16, len: u32) -> Result<u32> {
        let mut wrote = 0;
        if unsafe {
            FillConsoleOutputAttribute(
                self.handle.as_raw_handle() as *mut _,
                attr,
                len,
                COORD { X: x, Y: y },
                &mut wrote,
            )
        } == 0
        {
            bail!(
                "FillConsoleOutputAttribute failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(wrote)
    }

    fn set_attr(&mut self, attr: u16) -> Result<()> {
        if unsafe { SetConsoleTextAttribute(self.handle.as_raw_handle() as *mut _, attr) } == 0 {
            bail!(
                "SetConsoleTextAttribute failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(())
    }

    fn set_cursor_position(&mut self, x: i16, y: i16) -> Result<()> {
        if unsafe {
            SetConsoleCursorPosition(self.handle.as_raw_handle() as *mut _, COORD { X: x, Y: y })
        } == 0
        {
            bail!(
                "SetConsoleCursorPosition(x={}, y={}) failed: {}",
                x,
                y,
                IoError::last_os_error()
            );
        }
        Ok(())
    }

    fn get_buffer_contents(&mut self) -> Result<Vec<CHAR_INFO>> {
        let info = self.get_buffer_info()?;

        let cols = info.dwSize.X as usize;
        let rows = 1 + info.srWindow.Bottom as usize - info.srWindow.Top as usize;

        let mut res = vec![
            CHAR_INFO {
                Attributes: 0,
                Char: unsafe { mem::zeroed() }
            };
            cols * rows
        ];
        let mut read_region = SMALL_RECT {
            Left: 0,
            Right: info.dwSize.X - 1,
            Top: info.srWindow.Top,
            Bottom: info.srWindow.Bottom,
        };
        unsafe {
            if ReadConsoleOutputW(
                self.handle.as_raw_handle() as *mut _,
                res.as_mut_ptr(),
                COORD {
                    X: cols.min(i16::MAX as usize) as i16,
                    Y: rows.min(i16::MAX as usize) as i16,
                },
                COORD { X: 0, Y: 0 },
                &mut read_region,
            ) == 0
            {
                bail!("ReadConsoleOutputW failed: {}", IoError::last_os_error());
            }
        }
        Ok(res)
    }

    fn set_buffer_contents(&mut self, buffer: &[CHAR_INFO]) -> Result<()> {
        let info = self.get_buffer_info()?;

        let cols = info.dwSize.X as usize;
        let rows = 1 + info.srWindow.Bottom as usize - info.srWindow.Top as usize;
        ensure!(
            rows * cols == buffer.len(),
            "buffer size doesn't match screen size"
        );

        let mut write_region = SMALL_RECT {
            Left: 0,
            Right: info.dwSize.X - 1,
            Top: info.srWindow.Top,
            Bottom: info.srWindow.Bottom,
        };

        unsafe {
            if WriteConsoleOutputW(
                self.handle.as_raw_handle() as *mut _,
                buffer.as_ptr(),
                COORD {
                    X: cols.min(i16::MAX as usize) as i16,
                    Y: rows.min(i16::MAX as usize) as i16,
                },
                COORD { X: 0, Y: 0 },
                &mut write_region,
            ) == 0
            {
                bail!("WriteConsoleOutputW failed: {}", IoError::last_os_error());
            }
        }
        Ok(())
    }

    fn get_buffer_info(&mut self) -> Result<CONSOLE_SCREEN_BUFFER_INFO> {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { mem::zeroed() };
        let ok = unsafe {
            GetConsoleScreenBufferInfo(self.handle.as_raw_handle() as *mut _, &mut info as *mut _)
        };
        if ok == 0 {
            bail!(
                "GetConsoleScreenBufferInfo failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(info)
    }

    fn set_viewport(&mut self, left: i16, top: i16, right: i16, bottom: i16) -> Result<()> {
        let rect = SMALL_RECT {
            Left: left,
            Top: top,
            Right: right,
            Bottom: bottom,
        };
        if unsafe { SetConsoleWindowInfo(self.handle.as_raw_handle() as *mut _, 1, &rect) } == 0 {
            bail!("SetConsoleWindowInfo failed: {}", IoError::last_os_error());
        }
        Ok(())
    }

    fn scroll_region(
        &mut self,
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
        dx: i16,
        dy: i16,
        attr: u16,
    ) -> Result<()> {
        let scroll_rect = SMALL_RECT {
            Left: max(left, left - dx),
            Top: max(top, top - dy),
            Right: min(right, right - dx),
            Bottom: min(bottom, bottom - dy),
        };
        let clip_rect = SMALL_RECT {
            Left: left,
            Top: top,
            Right: right,
            Bottom: bottom,
        };
        let fill = unsafe {
            let mut fill = CHAR_INFO {
                Char: mem::zeroed(),
                Attributes: attr,
            };
            *fill.Char.UnicodeChar_mut() = ' ' as u16;
            fill
        };
        if unsafe {
            ScrollConsoleScreenBufferW(
                self.handle.as_raw_handle() as *mut _,
                &scroll_rect,
                &clip_rect,
                COORD {
                    X: max(left, left + dx),
                    Y: max(left, top + dy),
                },
                &fill,
            )
        } == 0
        {
            bail!(
                "ScrollConsoleScreenBufferW failed: {}",
                IoError::last_os_error()
            );
        }
        Ok(())
    }
}

pub struct WindowsTerminal {
    input_handle: InputHandle,
    output_handle: OutputHandle,
    waker_handle: Arc<EventHandle>,
    saved_input_mode: u32,
    saved_output_mode: u32,
    renderer: Renderer,
    input_parser: InputParser,
    input_queue: VecDeque<InputEvent>,
    saved_input_cp: u32,
    saved_output_cp: u32,
    in_alternate_screen: bool,
    alternate_main_output: Option<OutputHandle>,
    caps: Capabilities,
}

impl Drop for WindowsTerminal {
    fn drop(&mut self) {
        if matches!(&self.renderer, Renderer::Terminfo(_)) {
            macro_rules! decreset {
                ($variant:ident) => {
                    if let Err(err) = write!(
                        self.output_handle,
                        "{}",
                        CSI::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                            DecPrivateModeCode::$variant
                        )))
                    ) {
                        log::warn!(
                            "failed to reset terminal mode {} during WindowsTerminal drop: {err}",
                            stringify!($variant)
                        );
                    }
                };
            }
            self.render(&[Change::CursorVisibility(
                crate::surface::CursorVisibility::Visible,
            )])
            .ok();
            decreset!(BracketedPaste);
            decreset!(SGRMouse);
            decreset!(AnyEventMouse);
        }

        if let Err(err) = self.exit_alternate_screen() {
            log::warn!("failed to exit alternate screen during WindowsTerminal drop: {err}");
        }
        if let Err(err) = self.output_handle.flush() {
            log::warn!("failed to flush terminal during WindowsTerminal drop: {err}");
        }
        if let Err(err) = self.input_handle.set_input_mode(self.saved_input_mode) {
            log::warn!("failed to restore console input mode during WindowsTerminal drop: {err}");
        }
        if let Err(err) = self.input_handle.set_input_cp(self.saved_input_cp) {
            log::warn!(
                "failed to restore console input codepage during WindowsTerminal drop: {err}"
            );
        }
        if let Err(err) = self.output_handle.set_output_mode(self.saved_output_mode) {
            log::warn!("failed to restore console output mode during WindowsTerminal drop: {err}");
        }
        if let Err(err) = self.output_handle.set_output_cp(self.saved_output_cp) {
            log::warn!(
                "failed to restore console output codepage during WindowsTerminal drop: {err}"
            );
        }
    }
}

impl WindowsTerminal {
    /// Attempt to create an instance from the stdin and stdout of the
    /// process.  This will fail unless both are associated with a tty.
    /// Note that this will duplicate the underlying file descriptors
    /// and will no longer participate in the stdin/stdout locking
    /// provided by the rust standard library.
    pub fn new_from_stdio(caps: Capabilities) -> Result<Self> {
        Self::new_with(caps, stdin(), stdout())
    }

    /// Create an instance using the provided capabilities, read and write
    /// handles. The read and write handles must be tty handles of this
    /// will return an error.
    pub fn new_with<A: Read + IsTty + AsRawHandle, B: Write + IsTty + AsRawHandle>(
        caps: Capabilities,
        read: A,
        write: B,
    ) -> Result<Self> {
        if !read.is_tty() || !write.is_tty() {
            bail!("stdin and stdout must both be tty handles");
        }

        let mut input_handle = InputHandle {
            handle: FileDescriptor::dup(&read)?,
        };
        let mut output_handle = OutputHandle::new(FileDescriptor::dup(&write)?);
        let waker_handle = Arc::new(EventHandle::new()?);

        let saved_input_mode = input_handle.get_input_mode()?;
        let saved_output_mode = output_handle.get_output_mode()?;
        let saved_input_cp = input_handle.get_input_cp();
        let saved_output_cp = output_handle.get_output_cp();

        // Test whether we have a virtual terminal capable
        // console device by attempting to set the appropriate flags.
        let virtual_terminal_available = output_handle
            .set_output_mode(
                saved_output_mode
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                    | DISABLE_NEWLINE_AUTO_RETURN,
            )
            .is_ok();

        // Allow opting out of that processing
        fn bypass_virtual_terminal() -> bool {
            if let Ok(t) = std::env::var("TERMWIZ_BYPASS_VIRTUAL_TERMINAL") {
                t == "1"
            } else {
                false
            }
        }

        let renderer = if caps.terminfo_db().is_some() {
            Renderer::Terminfo(TerminfoRenderer::new(caps.clone()))
        } else if virtual_terminal_available && !bypass_virtual_terminal() {
            Renderer::Terminfo(TerminfoRenderer::new(caps.clone().apply_builtin_terminfo()))
        } else {
            Renderer::Windows(WindowsConsoleRenderer::new(caps.clone()))
        };
        let input_parser = InputParser::new();

        let mut terminal = Self {
            input_handle,
            output_handle,
            waker_handle,
            saved_input_mode,
            saved_output_mode,
            saved_input_cp,
            saved_output_cp,
            renderer,
            input_parser,
            input_queue: VecDeque::new(),
            in_alternate_screen: false,
            alternate_main_output: None,
            caps,
        };

        terminal.input_handle.set_input_cp(CP_UTF8)?;
        terminal.output_handle.set_output_cp(CP_UTF8)?;

        // We already enabled this for output, but let's also turn it
        // on for input here now.
        terminal.enable_virtual_terminal_processing_if_needed()?;

        Ok(terminal)
    }

    fn enable_virtual_terminal_processing_if_needed(&mut self) -> Result<()> {
        match &self.renderer {
            Renderer::Terminfo(_) => self.enable_virtual_terminal_processing(),
            Renderer::Windows(_) => Ok(()),
        }
    }

    /// Attempt to explicitly open handles to a console device (CONIN$,
    /// CONOUT$). This should yield the terminal already associated with
    /// the process, even if stdio streams have been redirected.
    pub fn new(caps: Capabilities) -> Result<Self> {
        let read = OpenOptions::new().read(true).write(true).open("CONIN$")?;
        let write = OpenOptions::new().read(true).write(true).open("CONOUT$")?;
        Self::new_with(caps, read, write)
    }

    pub fn enable_virtual_terminal_processing(&mut self) -> Result<()> {
        let mode = self.output_handle.get_output_mode()?;
        self.output_handle.set_output_mode(
            mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN,
        )?;

        let mode = self.input_handle.get_input_mode()?;
        self.input_handle
            .set_input_mode(mode | ENABLE_VIRTUAL_TERMINAL_INPUT)?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct WindowsTerminalWaker {
    handle: Option<Arc<EventHandle>>,
}

impl WindowsTerminalWaker {
    pub fn noop() -> Self {
        Self { handle: None }
    }

    pub fn wake(&self) -> IoResult<()> {
        let Some(handle) = self.handle.as_ref() else {
            return Ok(());
        };
        handle.set()?;
        Ok(())
    }
}

impl Terminal for WindowsTerminal {
    fn set_raw_mode(&mut self) -> Result<()> {
        let mode = self.output_handle.get_output_mode()?;
        self.output_handle
            .set_output_mode(mode | DISABLE_NEWLINE_AUTO_RETURN)
            .ok();

        let mode = self.input_handle.get_input_mode()?;

        self.input_handle.set_input_mode(
            (mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_MOUSE_INPUT
                | ENABLE_WINDOW_INPUT,
        )?;

        if matches!(&self.renderer, Renderer::Terminfo(_)) {
            macro_rules! decset {
                ($variant:ident) => {
                    write!(
                        self.output_handle,
                        "{}",
                        CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                            DecPrivateModeCode::$variant
                        )))
                    )?;
                };
            }

            if self.caps.bracketed_paste() {
                decset!(BracketedPaste);
            }
            if self.caps.mouse_reporting() {
                decset!(AnyEventMouse);
                decset!(SGRMouse);
            }
            self.output_handle.flush()?;
        }

        Ok(())
    }

    fn set_cooked_mode(&mut self) -> Result<()> {
        let mode = self.output_handle.get_output_mode()?;
        self.output_handle
            .set_output_mode(mode & !DISABLE_NEWLINE_AUTO_RETURN)
            .ok();

        let mode = self.input_handle.get_input_mode()?;

        self.input_handle.set_input_mode(
            (mode & !(ENABLE_MOUSE_INPUT | ENABLE_WINDOW_INPUT))
                | ENABLE_ECHO_INPUT
                | ENABLE_LINE_INPUT
                | ENABLE_PROCESSED_INPUT,
        )
    }

    fn enter_alternate_screen(&mut self) -> Result<()> {
        if matches!(&self.renderer, Renderer::Terminfo(_)) {
            if !self.in_alternate_screen {
                write!(
                    self.output_handle,
                    "{}",
                    CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                        DecPrivateModeCode::ClearAndEnableAlternateScreen
                    )))
                )?;
                self.in_alternate_screen = true;
            }
            Ok(())
        } else {
            if self.in_alternate_screen {
                return Ok(());
            }

            let current_info = self.output_handle.get_buffer_info()?;
            let current_mode = self.output_handle.get_output_mode()?;
            let (buffer_size, window_rect) = alternate_screen_buffer_geometry(current_info);

            let mut alternate = OutputHandle::new_screen_buffer()?;
            alternate.set_output_mode(current_mode)?;
            alternate.set_screen_buffer_size(buffer_size)?;
            alternate.set_viewport(
                window_rect.Left,
                window_rect.Top,
                window_rect.Right,
                window_rect.Bottom,
            )?;
            alternate.set_cursor_position(0, 0)?;
            alternate.set_active_screen_buffer()?;

            let main = mem::replace(&mut self.output_handle, alternate);
            self.alternate_main_output = Some(main);
            self.in_alternate_screen = true;
            Ok(())
        }
    }

    fn exit_alternate_screen(&mut self) -> Result<()> {
        if matches!(&self.renderer, Renderer::Terminfo(_)) {
            if self.in_alternate_screen {
                write!(
                    self.output_handle,
                    "{}",
                    CSI::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                        DecPrivateModeCode::ClearAndEnableAlternateScreen
                    )))
                )?;
                self.in_alternate_screen = false;
            }
            Ok(())
        } else if !self.in_alternate_screen {
            Ok(())
        } else {
            let Some(main_output) = self.alternate_main_output.as_mut() else {
                bail!("native Windows alternate screen is marked active but main buffer handle is missing");
            };
            main_output.set_active_screen_buffer()?;

            let main = self
                .alternate_main_output
                .take()
                .expect("checked alternate main output above");
            let alternate = mem::replace(&mut self.output_handle, main);
            drop(alternate);
            self.in_alternate_screen = false;
            Ok(())
        }
    }

    fn get_screen_size(&mut self) -> Result<ScreenSize> {
        let info = self.output_handle.get_buffer_info()?;
        let (cols, rows) = dimensions_from_buffer_info(info);

        Ok(ScreenSize {
            rows: cast(rows)?,
            cols: cast(cols)?,
            xpixel: 0,
            ypixel: 0,
        })
    }

    fn probe_capabilities(&mut self) -> Option<ProbeCapabilities> {
        Some(ProbeCapabilities::new(
            &mut self.input_handle,
            &mut self.output_handle,
        ))
    }

    fn set_screen_size(&mut self, size: ScreenSize) -> Result<()> {
        ensure!(
            size.cols > 0 && size.rows > 0,
            "screen size must be positive: cols={}, rows={}",
            size.cols,
            size.rows
        );
        let target = COORD {
            X: cast(size.cols)?,
            Y: cast(size.rows)?,
        };

        let plan = screen_resize_plan(self.output_handle.get_buffer_info()?, target);
        if let Some(rect) = plan.pre_buffer_viewport {
            self.output_handle
                .set_viewport(rect.Left, rect.Top, rect.Right, rect.Bottom)?;
        }
        self.output_handle
            .set_screen_buffer_size(plan.buffer_size)?;
        let rect = plan.final_viewport;
        self.output_handle
            .set_viewport(rect.Left, rect.Top, rect.Right, rect.Bottom)?;
        Ok(())
    }

    fn render(&mut self, changes: &[Change]) -> Result<()> {
        match &mut self.renderer {
            Renderer::Terminfo(r) => r.render_to(changes, &mut self.output_handle),
            Renderer::Windows(r) => r.render_to(changes, &mut self.output_handle),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.output_handle
            .flush()
            .map_err(|e| format_err!("flush failed: {}", e))
    }

    fn poll_input(&mut self, wait: Option<Duration>) -> Result<Option<InputEvent>> {
        loop {
            if let Some(event) = self.input_queue.pop_front() {
                return Ok(Some(event));
            }

            let mut pending = self.input_handle.get_number_of_input_events()?;

            if pending == 0 {
                let mut handles = [
                    self.input_handle.handle.as_raw_handle() as *mut _,
                    self.waker_handle.handle.as_raw_handle() as *mut _,
                ];
                let result = unsafe {
                    WaitForMultipleObjects(
                        2,
                        handles.as_mut_ptr(),
                        0,
                        wait.map(|wait| wait.as_millis() as u32).unwrap_or(INFINITE),
                    )
                };
                if result == WAIT_OBJECT_0 + 0 {
                    pending = self.input_handle.get_number_of_input_events()?;
                } else if result == WAIT_OBJECT_0 + 1 {
                    return Ok(Some(InputEvent::Wake));
                } else if result == WAIT_FAILED {
                    bail!(
                        "failed to WaitForMultipleObjects: {}",
                        IoError::last_os_error()
                    );
                } else if result == WAIT_TIMEOUT {
                    return Ok(None);
                } else {
                    return Ok(None);
                }
            }

            let records = self.input_handle.read_console_input(pending)?;

            let input_queue = &mut self.input_queue;
            self.input_parser
                .decode_input_records(&records, &mut |evt| input_queue.push_back(evt));
        }
    }

    fn waker(&self) -> WindowsTerminalWaker {
        WindowsTerminalWaker {
            handle: Some(self.waker_handle.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coord(cols: i16, rows: i16) -> COORD {
        COORD { X: cols, Y: rows }
    }

    fn rect_tuple(rect: SMALL_RECT) -> (i16, i16, i16, i16) {
        (rect.Left, rect.Top, rect.Right, rect.Bottom)
    }

    fn buffer_info(
        buffer_cols: i16,
        buffer_rows: i16,
        window_left: i16,
        window_top: i16,
        window_right: i16,
        window_bottom: i16,
    ) -> CONSOLE_SCREEN_BUFFER_INFO {
        CONSOLE_SCREEN_BUFFER_INFO {
            dwSize: COORD {
                X: buffer_cols,
                Y: buffer_rows,
            },
            dwCursorPosition: COORD { X: 0, Y: 0 },
            wAttributes: 0,
            srWindow: SMALL_RECT {
                Left: window_left,
                Top: window_top,
                Right: window_right,
                Bottom: window_bottom,
            },
            dwMaximumWindowSize: COORD {
                X: buffer_cols,
                Y: buffer_rows,
            },
        }
    }

    #[test]
    fn alternate_screen_geometry_preserves_main_buffer_scrollback_rows() {
        let info = buffer_info(120, 9_000, 0, 8_950, 119, 8_999);

        let (size, rect) = alternate_screen_buffer_geometry(info);

        assert_eq!((size.X, size.Y), (120, 9_000));
        assert_eq!(
            (rect.Left, rect.Top, rect.Right, rect.Bottom),
            (0, 0, 119, 49)
        );
    }

    #[test]
    fn alternate_screen_geometry_expands_buffer_to_visible_window() {
        let info = buffer_info(40, 10, 10, 5, 89, 29);

        let (size, rect) = alternate_screen_buffer_geometry(info);

        assert_eq!((size.X, size.Y), (80, 25));
        assert_eq!(
            (rect.Left, rect.Top, rect.Right, rect.Bottom),
            (0, 0, 79, 24)
        );
    }

    #[test]
    fn screen_resize_plan_shrinks_viewport_before_shrinking_buffer() {
        let info = buffer_info(120, 9_000, 0, 8_950, 119, 8_999);

        let plan = screen_resize_plan(info, coord(80, 24));

        let pre_buffer_viewport = plan
            .pre_buffer_viewport
            .expect("shrinking below the current viewport needs a pre-buffer viewport");
        assert_eq!(rect_tuple(pre_buffer_viewport), (0, 0, 79, 23));
        assert_eq!((plan.buffer_size.X, plan.buffer_size.Y), (80, 24));
        assert_eq!(rect_tuple(plan.final_viewport), (0, 0, 79, 23));
    }

    #[test]
    fn screen_resize_plan_grows_buffer_before_growing_viewport() {
        let info = buffer_info(80, 24, 0, 0, 79, 23);

        let plan = screen_resize_plan(info, coord(120, 40));

        assert!(plan.pre_buffer_viewport.is_none());
        assert_eq!((plan.buffer_size.X, plan.buffer_size.Y), (120, 40));
        assert_eq!(rect_tuple(plan.final_viewport), (0, 0, 119, 39));
    }

    #[test]
    fn screen_resize_plan_clamps_offset_viewport_inside_target_buffer() {
        let info = buffer_info(200, 100, 20, 10, 119, 39);

        let plan = screen_resize_plan(info, coord(80, 50));

        let pre_buffer_viewport = plan
            .pre_buffer_viewport
            .expect("width shrink needs the viewport to fit inside the target buffer first");
        assert_eq!(rect_tuple(pre_buffer_viewport), (0, 10, 79, 39));
        assert_eq!((plan.buffer_size.X, plan.buffer_size.Y), (80, 50));
        assert_eq!(rect_tuple(plan.final_viewport), (0, 0, 79, 49));
    }
}
