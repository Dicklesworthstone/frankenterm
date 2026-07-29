use crate::os::x11::xcb_util::*;
use crate::x11::XConnection;
use crate::MouseCursor;
use anyhow::{ensure, Context};
use config::ConfigHandle;
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::ffi::OsStr;
use std::io::prelude::*;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use xcb::x::Cursor;
use xcb::Xid;

// X11 classic Cursor glyphs
pub const HAND1: u16 = 58;
pub const SB_H_DOUBLE_ARROW: u16 = 108;
pub const SB_V_DOUBLE_ARROW: u16 = 116;
pub const TOP_LEFT_ARROW: u16 = 132;
pub const TOP_LEFT_CORNER: u16 = 134;
pub const XTERM: u16 = 152;

const XCURSOR_IMAGE_MAX_DIMENSION: u16 = 0x7fff;
const XCURSOR_IMAGE_MAX_BYTES: usize = 64 * 1024 * 1024;
const XCURSOR_MAGIC: u32 = 0x7275_6358;
const XCURSOR_IMAGE_TYPE: u32 = 0xfffd_0002;
const XCURSOR_FILE_HEADER_BASE_BYTES: u32 = 16;
const XCURSOR_IMAGE_HEADER_BASE_BYTES: u32 = 36;
const XCURSOR_MAX_TOC_ENTRIES: u32 = 0x1_0000;

#[derive(Debug)]
struct XcursorToc {
    type_: u32,
    subtype: u32,
    position: u32,
}

struct XcursorImage {
    width: u16,
    height: u16,
    xhot: u16,
    yhot: u16,
    pixels: Vec<u8>,
}

/// Read a u32 stored in Xcursor's little-endian file representation.
fn read_xcursor_u32(reader: &mut impl Read) -> anyhow::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_xcursor_toc(reader: &mut (impl Read + Seek)) -> anyhow::Result<Vec<XcursorToc>> {
    let magic = read_xcursor_u32(reader)?;
    let header_bytes = read_xcursor_u32(reader)?;
    let _version = read_xcursor_u32(reader)?;
    let ntoc = read_xcursor_u32(reader)?;

    ensure!(
        magic == XCURSOR_MAGIC,
        "magic number doesn't match 0x{magic:x} != expected 0x{XCURSOR_MAGIC:x}"
    );
    ensure!(
        header_bytes >= XCURSOR_FILE_HEADER_BASE_BYTES,
        "Xcursor file header is {header_bytes} bytes, shorter than the \
         {XCURSOR_FILE_HEADER_BASE_BYTES}-byte base header"
    );
    ensure!(
        ntoc <= XCURSOR_MAX_TOC_ENTRIES,
        "Xcursor table of contents has {ntoc} entries, exceeding the \
         {XCURSOR_MAX_TOC_ENTRIES}-entry format limit"
    );

    // The declared header length includes any forward-compatible extension
    // fields. TOC entries begin after those fields, not necessarily at byte 16.
    reader.seek(SeekFrom::Start(u64::from(header_bytes)))?;

    let ntoc = usize::try_from(ntoc).context("Xcursor TOC entry count does not fit in usize")?;
    let mut toc = Vec::new();
    toc.try_reserve_exact(ntoc)
        .context("failed to allocate Xcursor table of contents")?;
    for _ in 0..ntoc {
        toc.push(XcursorToc {
            type_: read_xcursor_u32(reader)?,
            subtype: read_xcursor_u32(reader)?,
            position: read_xcursor_u32(reader)?,
        });
    }
    ensure!(!toc.is_empty(), "no images are present");
    Ok(toc)
}

fn read_xcursor_image(
    reader: &mut (impl Read + Seek),
    item: &XcursorToc,
) -> anyhow::Result<XcursorImage> {
    reader.seek(SeekFrom::Start(u64::from(item.position)))?;

    let chunk_header_bytes = read_xcursor_u32(reader)?;
    let chunk_type = read_xcursor_u32(reader)?;
    let chunk_subtype = read_xcursor_u32(reader)?;
    let _chunk_version = read_xcursor_u32(reader)?;

    ensure!(
        chunk_header_bytes >= XCURSOR_IMAGE_HEADER_BASE_BYTES,
        "Xcursor image header is {chunk_header_bytes} bytes, shorter than the \
         {XCURSOR_IMAGE_HEADER_BASE_BYTES}-byte base header"
    );
    ensure!(
        chunk_type == item.type_,
        "chunk_type {chunk_type:x} != item.type_ {:x}",
        item.type_
    );
    ensure!(
        chunk_subtype == item.subtype,
        "chunk_subtype {chunk_subtype:x} != item.subtype {:x}",
        item.subtype
    );

    let width = read_xcursor_u32(reader)?;
    let height = read_xcursor_u32(reader)?;
    let xhot = read_xcursor_u32(reader)?;
    let yhot = read_xcursor_u32(reader)?;
    let _delay = read_xcursor_u32(reader)?;

    ensure!(
        width > 0 && height > 0,
        "cursor image dimensions must be non-zero"
    );
    ensure!(
        width <= u32::from(XCURSOR_IMAGE_MAX_DIMENSION)
            && height <= u32::from(XCURSOR_IMAGE_MAX_DIMENSION),
        "cursor image dimensions {width}x{height} exceed the Xcursor format maximum of \
         {XCURSOR_IMAGE_MAX_DIMENSION}"
    );
    ensure!(
        xhot <= width && yhot <= height,
        "cursor hotspot ({xhot}, {yhot}) is outside {width}x{height} image"
    );

    let width = u16::try_from(width).context("cursor width does not fit in u16")?;
    let height = u16::try_from(height).context("cursor height does not fit in u16")?;
    let xhot = u16::try_from(xhot).context("cursor x hotspot does not fit in u16")?;
    let yhot = u16::try_from(yhot).context("cursor y hotspot does not fit in u16")?;
    let num_pixels = usize::from(width)
        .checked_mul(usize::from(height))
        .context("cursor pixel count overflow")?;
    let pixel_bytes = num_pixels
        .checked_mul(4)
        .context("cursor byte count overflow")?;
    ensure!(
        pixel_bytes <= XCURSOR_IMAGE_MAX_BYTES,
        "cursor image requires {pixel_bytes} bytes, exceeding the \
         {XCURSOR_IMAGE_MAX_BYTES}-byte safety limit"
    );

    // The image header length likewise includes extension fields. Pixel words
    // begin at the declared boundary rather than immediately after the base
    // fields read above.
    let pixel_offset = u64::from(item.position)
        .checked_add(u64::from(chunk_header_bytes))
        .context("Xcursor image pixel offset overflow")?;
    reader.seek(SeekFrom::Start(pixel_offset))?;

    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(pixel_bytes)
        .context("failed to allocate cursor pixel storage")?;
    pixels.resize(pixel_bytes, 0);
    reader.read_exact(&mut pixels)?;

    Ok(XcursorImage {
        width,
        height,
        xhot,
        yhot,
        pixels,
    })
}

pub struct XcbCursor {
    pub id: Cursor,
    pub conn: Weak<XConnection>,
}

impl Drop for XcbCursor {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.upgrade() {
            conn.send_request_no_reply_log(&xcb::x::FreeCursor { cursor: self.id });
        }
    }
}

pub struct CursorInfo {
    cursors: HashMap<Option<MouseCursor>, XcbCursor>,
    cursor: Option<MouseCursor>,
    conn: Weak<XConnection>,
    size: Option<u32>,
    theme: Option<String>,
    icon_path: Vec<PathBuf>,
    pict_format_id: Option<xcb::render::Pictformat>,
}

fn icon_path() -> Vec<PathBuf> {
    let path = match std::env::var_os("XCURSOR_PATH") {
        Some(path) => {
            log::trace!("Using $XCURSOR_PATH icon path: {:?}", path);
            path
        }
        None => {
            log::trace!("Constructing default icon path because $XCURSOR_PATH is not set");

            fn add_icons_dir(path: &OsStr, dest: &mut Vec<PathBuf>) {
                for entry in std::env::split_paths(path) {
                    dest.push(entry.join("icons"));
                }
            }

            fn xdg_location(name: &str, def: &str, dest: &mut Vec<PathBuf>) {
                if let Some(var) = std::env::var_os(name) {
                    log::trace!("Using ${} location {:?}", name, var);
                    add_icons_dir(&var, dest);
                } else {
                    log::trace!("Using {} because ${} is not set", def, name);
                    add_icons_dir(OsStr::new(def), dest);
                }
            }

            let mut path = vec![];
            xdg_location("XDG_DATA_HOME", "~/.local/share", &mut path);
            path.push("~/.icons".into());
            xdg_location("XDG_DATA_DIRS", "/usr/local/share:/usr/share", &mut path);
            path.push("/usr/share/pixmaps".into());
            path.push("~/.cursors".into());
            path.push("/usr/share/cursors/xorg-x11".into());
            path.push("/usr/X11R6/lib/X11/icons".into());

            std::env::join_paths(path).expect("failed to compose default xcursor path")
        }
    };

    fn tilde_expand(p: PathBuf) -> PathBuf {
        match p.to_str() {
            Some(s) => {
                if let Some(stripped) = s.strip_prefix("~/") {
                    if let Some(home) = dirs_next::home_dir() {
                        home.join(stripped)
                    } else {
                        p
                    }
                } else {
                    p
                }
            }
            None => p,
        }
    }

    std::env::split_paths(&path).map(tilde_expand).collect()
}

fn cursor_size(xcursor_size: &Option<u32>, map: &HashMap<String, String>) -> u32 {
    if let Some(size) = xcursor_size {
        return *size;
    }

    if let Ok(size) = std::env::var("XCURSOR_SIZE") {
        if let Ok(size) = size.parse::<u32>() {
            return size;
        }
    }

    if let Some(size) = map.get("Xcursor.size") {
        if let Ok(size) = size.parse::<u32>() {
            return size;
        }
    }

    if let Some(dpi) = map.get("Xft.dpi") {
        if let Ok(dpi) = dpi.parse::<u32>() {
            return dpi * 16 / 72;
        }
    }

    // Probably a good default?
    24
}

impl CursorInfo {
    pub fn new(config: &ConfigHandle, conn: &Rc<XConnection>) -> Self {
        let mut size = None;
        let mut theme = None;
        let mut pict_format_id = None;
        // If we know the theme to use, then we need the render extension
        // if we are to be able to load the cursor
        let has_render = conn
            .active_extensions()
            .any(|e| e == xcb::Extension::Render);
        if has_render {
            if let Ok(vers) = conn.send_and_wait_request(&xcb::render::QueryVersion {
                client_major_version: xcb::render::MAJOR_VERSION,
                client_minor_version: xcb::render::MINOR_VERSION,
            }) {
                // 0.5 and later have the required support
                if (vers.major_version(), vers.minor_version()) >= (0, 5) {
                    size.replace(cursor_size(&config.xcursor_size, &conn.xrm.borrow()));
                    theme = config
                        .xcursor_theme
                        .as_ref()
                        .map(|s| s.to_string())
                        .or_else(|| conn.xrm.borrow().get("Xcursor.theme").cloned());

                    // Locate the Pictformat corresponding to ARGB32
                    if let Ok(formats) =
                        conn.send_and_wait_request(&xcb::render::QueryPictFormats {})
                    {
                        for fmt in formats.formats() {
                            if fmt.depth() == 32 {
                                let direct = fmt.direct();
                                if direct.alpha_shift == 24
                                    && direct.red_shift == 16
                                    && direct.green_shift == 8
                                    && direct.blue_shift == 0
                                {
                                    pict_format_id.replace(fmt.id());
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        let icon_path = icon_path();
        log::trace!("icon_path is {:?}", icon_path);

        Self {
            cursors: HashMap::new(),
            cursor: None,
            conn: Rc::downgrade(conn),
            size,
            theme,
            icon_path,
            pict_format_id,
        }
    }

    fn conn(&self) -> anyhow::Result<Rc<XConnection>> {
        self.conn
            .upgrade()
            .context("XConnection is unavailable for cursor update")
    }

    pub fn set_cursor(
        &mut self,
        window_id: xcb::x::Window,
        cursor: Option<MouseCursor>,
    ) -> anyhow::Result<()> {
        if cursor == self.cursor {
            return Ok(());
        }

        let conn = self.conn()?;

        let cursor_id = match self.cursors.get(&cursor) {
            Some(cursor) => cursor.id,
            None => match self.load_themed(&conn, cursor) {
                Some(c) => c,
                None => self.load_basic(&conn, cursor),
            },
        };

        conn.send_request_no_reply(&xcb::x::ChangeWindowAttributes {
            window: window_id,
            value_list: &[xcb::x::Cw::Cursor(cursor_id)],
        })
        .context("set_cursor")?;

        self.cursor = cursor;

        Ok(())
    }

    fn create_blank(&mut self, conn: &Rc<XConnection>) -> anyhow::Result<Cursor> {
        let mut pixels = [0u8; 4];

        let image = XcbImage::create_native(
            conn,
            1,
            1,
            xcb::x::ImageFormat::ZPixmap as u32,
            32,
            std::ptr::null_mut(),
            pixels.len() as u32,
            pixels.as_mut_ptr(),
        )?;

        let pixmap = conn.generate_id();
        conn.send_request_no_reply(&xcb::x::CreatePixmap {
            depth: 32,
            pid: pixmap,
            drawable: xcb::x::Drawable::Window(conn.root),
            width: 1,
            height: 1,
        })
        .context("CreatePixmap")?;

        let gc = conn.generate_id();
        conn.send_request_no_reply(&xcb::x::CreateGc {
            cid: gc,
            drawable: xcb::x::Drawable::Pixmap(pixmap),
            value_list: &[],
        })
        .context("CreateGc")?;

        image.put(conn, pixmap.resource_id(), gc.resource_id(), 0, 0, 0);

        conn.send_request(&xcb::x::FreeGc { gc });

        let pict_format_id = self
            .pict_format_id
            .context("missing ARGB32 X render pictformat for blank cursor")?;

        let pic = conn.generate_id();
        conn.send_request_no_reply(&xcb::render::CreatePicture {
            pid: pic,
            drawable: xcb::x::Drawable::Pixmap(pixmap),
            format: pict_format_id,
            value_list: &[],
        })
        .context("create_picture")?;

        conn.send_request(&xcb::x::FreePixmap { pixmap });

        let cursor_id: Cursor = conn.generate_id();
        conn.send_request_no_reply(&xcb::render::CreateCursor {
            cid: cursor_id,
            source: pic,
            x: 0,
            y: 0,
        })
        .context("create_cursor")?;

        conn.send_request_no_reply(&xcb::render::FreePicture { picture: pic })
            .context("FreePicture")?;

        Ok(cursor_id)
    }

    fn load_themed(
        &mut self,
        conn: &Rc<XConnection>,
        cursor: Option<MouseCursor>,
    ) -> Option<Cursor> {
        if cursor.is_none() {
            match self.create_blank(conn) {
                Ok(cursor_id) => {
                    self.cursors.insert(
                        cursor,
                        XcbCursor {
                            id: cursor_id,
                            conn: Rc::downgrade(conn),
                        },
                    );
                    return Some(cursor_id);
                }
                Err(err) => {
                    log::error!("Failed to create blank cursor: {:#}", err);
                    return self.load_themed(conn, Some(MouseCursor::Arrow));
                }
            }
        }

        let theme = self.theme.as_deref().unwrap_or("default");
        self.pict_format_id?;

        let names: &[&str] = match cursor.unwrap_or(MouseCursor::Arrow) {
            MouseCursor::Arrow => &["top_left_arrow", "left_ptr"],
            MouseCursor::Hand => &["hand2"],
            MouseCursor::Text => &["xterm"],
            MouseCursor::SizeUpDown => &["sb_v_double_arrow"],
            MouseCursor::SizeLeftRight => &["sb_h_double_arrow"],
        };

        let mut theme_list = vec![theme.to_string()];
        let mut visited = HashSet::new();

        while !theme_list.is_empty() {
            let theme = theme_list.remove(0);
            if visited.contains(&theme) {
                continue;
            }

            visited.insert(theme.clone());

            for dir in &self.icon_path {
                for name in names {
                    let candidate = dir.join(&theme).join("cursors").join(name);
                    log::trace!(
                        "candidate for theme={theme} {:?} is {:?}",
                        cursor,
                        candidate
                    );
                    if let Ok(file) = std::fs::File::open(&candidate) {
                        match self.parse_cursor_file(conn, file) {
                            Ok(cursor_id) => {
                                self.cursors.insert(
                                    cursor,
                                    XcbCursor {
                                        id: cursor_id,
                                        conn: Rc::downgrade(conn),
                                    },
                                );

                                log::trace!("{:?} resolved to {:?}", cursor, candidate);
                                return Some(cursor_id);
                            }
                            Err(err) => log::error!("{:#}", err),
                        }
                    }
                }

                let theme_index = dir.join(&theme).join("index.theme");
                if let Some(inherited) = extract_inherited_theme_name(theme_index) {
                    log::trace!("theme {theme} inherits from theme {inherited}");
                    theme_list.push(inherited);
                }
            }
        }
        None
    }

    fn load_basic(&mut self, conn: &Rc<XConnection>, cursor: Option<MouseCursor>) -> Cursor {
        let id_no = match cursor.unwrap_or(MouseCursor::Arrow) {
            // `/usr/include/X11/cursorfont.h`
            // <https://docs.rs/xcb-util/0.3.0/src/xcb_util/cursor.rs.html>
            MouseCursor::Arrow => TOP_LEFT_ARROW,
            MouseCursor::Hand => HAND1,
            MouseCursor::Text => XTERM,
            MouseCursor::SizeUpDown => SB_V_DOUBLE_ARROW,
            MouseCursor::SizeLeftRight => SB_H_DOUBLE_ARROW,
        };
        log::trace!("loading X11 basic cursor {} for {:?}", id_no, cursor);

        let cursor_id: Cursor = conn.generate_id();
        conn.send_request_no_reply_log(&xcb::x::CreateGlyphCursor {
            cid: cursor_id,
            source_font: conn.cursor_font_id,
            mask_font: conn.cursor_font_id,
            source_char: id_no,
            mask_char: id_no + 1,
            fore_red: 0xffff,
            fore_green: 0xffff,
            fore_blue: 0xffff,
            back_red: 0,
            back_green: 0,
            back_blue: 0,
        });

        self.cursors.insert(
            cursor,
            XcbCursor {
                id: cursor_id,
                conn: Rc::downgrade(conn),
            },
        );

        cursor_id
    }

    fn parse_cursor_file(
        &self,
        conn: &Rc<XConnection>,
        mut file: std::fs::File,
    ) -> anyhow::Result<Cursor> {
        /* See: <https://cgit.freedesktop.org/xcb/util-cursor/tree/cursor/load_cursor.c>
         *
         * Cursor files start with a header.  The header
         * contains a magic number, a version number and a
         * table of contents which has type and offset information
         * for the remaining tables in the file.
         *
         * File minor versions increment for compatible changes
         * File major versions increment for incompatible changes (never, we hope)
         *
         * Chunks of the same type are always upward compatible.  Incompatible
         * changes are made with new chunk types; the old data can remain under
         * the old type.  Upward compatible changes can add header data as the
         * header lengths are specified in the file.
         *
         *  File:
         *      FileHeader
         *      LISTofChunk
         *
         *  FileHeader:
         *      CARD32          magic       magic number
         *      CARD32          header      bytes in file header
         *      CARD32          version     file version
         *      CARD32          ntoc        number of toc entries
         *      LISTofFileToc   toc         table of contents
         *
         *  FileToc:
         *      CARD32          type        entry type
         *      CARD32          subtype     entry subtype (size for images)
         *      CARD32          position    absolute file position
         */

        let toc = read_xcursor_toc(&mut file)?;

        let size = i64::from(self.size.unwrap_or(24));
        let mut best = None;
        for item in &toc {
            if item.type_ != XCURSOR_IMAGE_TYPE {
                continue;
            }
            let distance = (i64::from(item.subtype) - size).abs();
            match best.take() {
                None => {
                    best.replace((item, distance));
                }
                Some((other_item, other_dist)) => {
                    best.replace(if distance < other_dist {
                        (item, distance)
                    } else {
                        (other_item, other_dist)
                    });
                }
            }
        }

        let item = best
            .take()
            .ok_or_else(|| anyhow::anyhow!("no matching images"))?
            .0;

        let XcursorImage {
            width,
            height,
            xhot,
            yhot,
            mut pixels,
        } = read_xcursor_image(&mut file, item)?;
        let pixel_bytes_u32: u32 = pixels
            .len()
            .try_into()
            .context("cursor image byte count does not fit in u32")?;

        xcursor_pixels_to_image_byte_order(&mut pixels, conn.get_setup().image_byte_order());

        let image = XcbImage::create_native(
            conn,
            width,
            height,
            xcb::x::ImageFormat::ZPixmap as u32,
            32,
            std::ptr::null_mut(),
            pixel_bytes_u32,
            pixels.as_mut_ptr(),
        )?;

        let pixmap = conn.generate_id();
        conn.send_request_no_reply(&xcb::x::CreatePixmap {
            depth: 32,
            pid: pixmap,
            drawable: xcb::x::Drawable::Window(conn.root),
            width,
            height,
        })
        .context("create_pixmap")?;

        let gc = conn.generate_id();
        conn.send_request_no_reply(&xcb::x::CreateGc {
            cid: gc,
            drawable: xcb::x::Drawable::Pixmap(pixmap),
            value_list: &[],
        })
        .context("CreateGc")?;

        image.put(conn, pixmap.resource_id(), gc.resource_id(), 0, 0, 0);

        conn.send_request_no_reply(&xcb::x::FreeGc { gc })?;

        let pict_format_id = self
            .pict_format_id
            .context("missing ARGB32 X render pictformat for themed cursor")?;

        let pic = conn.generate_id();
        conn.send_request_no_reply(&xcb::render::CreatePicture {
            pid: pic,
            drawable: xcb::x::Drawable::Pixmap(pixmap),
            format: pict_format_id,
            value_list: &[],
        })
        .context("create_picture")?;

        conn.send_request_no_reply(&xcb::x::FreePixmap { pixmap })?;

        let cursor_id: Cursor = conn.generate_id();
        conn.send_request_no_reply(&xcb::render::CreateCursor {
            cid: cursor_id,
            source: pic,
            x: xhot,
            y: yhot,
        })
        .context("create_cursor")?;

        conn.send_request_no_reply(&xcb::render::FreePicture { picture: pic })?;

        Ok(cursor_id)
    }
}

fn xcursor_pixels_to_image_byte_order(pixels: &mut [u8], image_order: xcb::x::ImageOrder) {
    let (pixels, remainder) = pixels.as_chunks_mut::<4>();
    debug_assert!(
        remainder.is_empty(),
        "Xcursor pixel storage must contain whole u32 values"
    );
    for pixel in pixels {
        let value = u32::from_le_bytes(*pixel);
        *pixel = match image_order {
            xcb::x::ImageOrder::LsbFirst => value.to_le_bytes(),
            xcb::x::ImageOrder::MsbFirst => value.to_be_bytes(),
        };
    }
}

// The index.theme file looks something like this:
//
// [Icon Theme]
// Inherits=Adwaita
//
// This function extracts the inherited theme name from it.
fn extract_inherited_theme_name(p: PathBuf) -> Option<String> {
    let data = std::fs::read_to_string(&p).ok()?;
    log::trace!("Parsing {p:?} to determine inheritance");
    for line in data.lines() {
        let fields: Vec<&str> = line.splitn(2, '=').collect();
        if fields.len() == 2 {
            let key = fields[0].trim();
            if key == "Inherits" {
                fn separator(c: char) -> bool {
                    c.is_whitespace() || c == ';' || c == ','
                }

                return Some(
                    fields[1]
                        .trim()
                        .chars()
                        .skip_while(|&c| separator(c))
                        .take_while(|&c| !separator(c))
                        .collect(),
                );
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        read_xcursor_image, read_xcursor_toc, xcursor_pixels_to_image_byte_order, XcursorToc,
        XCURSOR_IMAGE_HEADER_BASE_BYTES, XCURSOR_IMAGE_TYPE, XCURSOR_MAGIC,
        XCURSOR_MAX_TOC_ENTRIES,
    };
    use std::io::Cursor as IoCursor;

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn image_chunk(
        header_bytes: u32,
        width: u32,
        height: u32,
        xhot: u32,
        yhot: u32,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_u32(&mut bytes, header_bytes);
        push_u32(&mut bytes, XCURSOR_IMAGE_TYPE);
        push_u32(&mut bytes, 24);
        push_u32(&mut bytes, 1);
        push_u32(&mut bytes, width);
        push_u32(&mut bytes, height);
        push_u32(&mut bytes, xhot);
        push_u32(&mut bytes, yhot);
        push_u32(&mut bytes, 0);
        assert_eq!(bytes.len(), XCURSOR_IMAGE_HEADER_BASE_BYTES as usize);
        bytes.resize(header_bytes as usize, 0xa5);
        bytes.extend_from_slice(pixels);
        bytes
    }

    #[test]
    fn xcursor_pixel_words_follow_the_connected_x_server_byte_order() {
        let little_endian = [0x44, 0x33, 0x22, 0x11, 0xdd, 0xcc, 0xbb, 0xaa];

        let mut lsb_first = little_endian;
        xcursor_pixels_to_image_byte_order(&mut lsb_first, xcb::x::ImageOrder::LsbFirst);
        assert_eq!(lsb_first, little_endian);

        let mut msb_first = little_endian;
        xcursor_pixels_to_image_byte_order(&mut msb_first, xcb::x::ImageOrder::MsbFirst);
        assert_eq!(msb_first, [0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xdd]);
    }

    #[test]
    fn xcursor_extended_file_and_image_headers_are_skipped() {
        let mut bytes = Vec::new();
        push_u32(&mut bytes, XCURSOR_MAGIC);
        push_u32(&mut bytes, 20);
        push_u32(&mut bytes, 1);
        push_u32(&mut bytes, 1);
        push_u32(&mut bytes, 0xfeed_face);

        push_u32(&mut bytes, XCURSOR_IMAGE_TYPE);
        push_u32(&mut bytes, 24);
        push_u32(&mut bytes, 32);
        assert_eq!(bytes.len(), 32);

        bytes.extend_from_slice(&image_chunk(40, 1, 1, 0, 0, &[0x44, 0x33, 0x22, 0x11]));

        let mut reader = IoCursor::new(bytes);
        let toc = read_xcursor_toc(&mut reader).expect("extended file header should parse");
        assert_eq!(toc.len(), 1);
        let image =
            read_xcursor_image(&mut reader, &toc[0]).expect("extended image header should parse");
        assert_eq!(
            (image.width, image.height, image.xhot, image.yhot),
            (1, 1, 0, 0)
        );
        assert_eq!(image.pixels, [0x44, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn xcursor_rejects_oversized_toc_before_reading_entries() {
        let mut bytes = Vec::new();
        push_u32(&mut bytes, XCURSOR_MAGIC);
        push_u32(&mut bytes, 16);
        push_u32(&mut bytes, 1);
        push_u32(&mut bytes, XCURSOR_MAX_TOC_ENTRIES + 1);

        let error = read_xcursor_toc(&mut IoCursor::new(bytes))
            .expect_err("oversized TOC must be rejected");
        assert!(
            error.to_string().contains("exceeding"),
            "unexpected rejection: {:#}",
            error
        );
    }

    #[test]
    fn xcursor_geometry_matches_reference_hotspot_and_memory_bounds() {
        let item = XcursorToc {
            type_: XCURSOR_IMAGE_TYPE,
            subtype: 24,
            position: 0,
        };

        let mut edge_hotspot = IoCursor::new(image_chunk(
            XCURSOR_IMAGE_HEADER_BASE_BYTES,
            1,
            1,
            1,
            1,
            &[0; 4],
        ));
        let image = read_xcursor_image(&mut edge_hotspot, &item)
            .expect("reference Xcursor accepts a hotspot equal to the dimensions");
        assert_eq!((image.xhot, image.yhot), (1, 1));

        let mut outside_hotspot = IoCursor::new(image_chunk(
            XCURSOR_IMAGE_HEADER_BASE_BYTES,
            1,
            1,
            2,
            1,
            &[0; 4],
        ));
        let error = read_xcursor_image(&mut outside_hotspot, &item)
            .err()
            .expect("hotspot beyond the dimensions must be rejected");
        assert!(error.to_string().contains("outside"));

        let mut oversized_pixels = IoCursor::new(image_chunk(
            XCURSOR_IMAGE_HEADER_BASE_BYTES,
            4097,
            4096,
            0,
            0,
            &[],
        ));
        let error = read_xcursor_image(&mut oversized_pixels, &item)
            .err()
            .expect("cursor allocation beyond the safety cap must be rejected");
        assert!(error.to_string().contains("safety limit"));
    }
}
