//! Render snapshots for the renderer image-parity corpus (ft-yccm0.1.10).
//!
//! When `FRANKENTERM_RENDER_SNAPSHOT` names a PNG path, the first paint after
//! the active pane's title becomes the sentinel
//! (`FRANKENTERM_RENDER_SNAPSHOT_TITLE`, default `ft-render-snapshot`) draws
//! the frame into an offscreen texture of the surface's format and size, with
//! the same geometry and pipeline as a presented frame, and writes it there.
//! A scene prints its bytes and then sets that title (`OSC 2`), and terminal
//! output is applied in order, so the snapshot holds the whole scene as the
//! real renderer draws it: fonts, shaping, glyph atlas, shaders and chrome.
//! Drawing offscreen means a hidden or occluded window, whose surface yields
//! no drawable, still captures.
//!
//! The variables are read once at window creation; with neither set this
//! module does nothing. A snapshot is taken once per window. The snapshot
//! paint presents nothing and settles no damage, so the next paint draws the
//! window as usual.
//!
//! Some window state cannot be produced by terminal bytes alone. Optional
//! actions set it up (ft-yccm0.1.10):
//!
//! - `FRANKENTERM_RENDER_SNAPSHOT_FOCUS=1` renders the window as focused.
//!   The corpus runs windows that never take keyboard focus from the
//!   operator, so without this every cursor is the unfocused hollow block.
//!   Only the window's own focus state changes; nothing is activated.
//! - `FRANKENTERM_RENDER_SNAPSHOT_SELECTION=r0,c0,r1,c1` selects from
//!   visible row `r0`, column `c0` through row `r1`, column `c1` (inclusive)
//!   in the active pane.
//! - `FRANKENTERM_RENDER_SNAPSHOT_SPLIT=right|bottom` together with
//!   `FRANKENTERM_RENDER_SNAPSHOT_SPLIT_COMMAND` splits the active pane when
//!   its title becomes `ft-render-split`, running the command (`/bin/sh -c`)
//!   in the new pane; that pane then sets the snapshot title.
//!
//! Focus and selection are applied in the paint that first sees the
//! snapshot title, and the snapshot is taken by the next paint.

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) const SNAPSHOT_PATH_ENV: &str = "FRANKENTERM_RENDER_SNAPSHOT";
pub(crate) const SNAPSHOT_TITLE_ENV: &str = "FRANKENTERM_RENDER_SNAPSHOT_TITLE";
pub(crate) const SNAPSHOT_FOCUS_ENV: &str = "FRANKENTERM_RENDER_SNAPSHOT_FOCUS";
pub(crate) const SNAPSHOT_SELECTION_ENV: &str = "FRANKENTERM_RENDER_SNAPSHOT_SELECTION";
pub(crate) const SNAPSHOT_SPLIT_ENV: &str = "FRANKENTERM_RENDER_SNAPSHOT_SPLIT";
pub(crate) const SNAPSHOT_SPLIT_COMMAND_ENV: &str = "FRANKENTERM_RENDER_SNAPSHOT_SPLIT_COMMAND";
pub(crate) const DEFAULT_SNAPSHOT_TITLE: &str = "ft-render-snapshot";
/// The title that triggers a requested split.
pub(crate) const SPLIT_TITLE: &str = "ft-render-split";
const READBACK_TIMEOUT: Duration = Duration::from_secs(10);
const READBACK_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A selection in visible cell coordinates, both ends inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnapshotSelection {
    pub(crate) start_row: usize,
    pub(crate) start_col: usize,
    pub(crate) end_row: usize,
    pub(crate) end_col: usize,
}

impl SnapshotSelection {
    fn parse(text: &str) -> Result<Self, String> {
        let numbers = text
            .split(',')
            .map(|part| part.trim().parse::<usize>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| format!("{SNAPSHOT_SELECTION_ENV}={text:?} is not r0,c0,r1,c1"))?;
        let [start_row, start_col, end_row, end_col] = numbers[..] else {
            return Err(format!(
                "{SNAPSHOT_SELECTION_ENV}={text:?} is not r0,c0,r1,c1"
            ));
        };
        if (end_row, end_col) < (start_row, start_col) {
            return Err(format!(
                "{SNAPSHOT_SELECTION_ENV}={text:?} ends before it starts"
            ));
        }
        Ok(Self {
            start_row,
            start_col,
            end_row,
            end_col,
        })
    }
}

/// Where a requested split puts the new pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotSplitDirection {
    Right,
    Bottom,
}

/// A split to perform before the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotSplit {
    pub(crate) direction: SnapshotSplitDirection,
    /// Run as `/bin/sh -c <command>` in the new pane.
    pub(crate) command: String,
}

/// Window state to set up before the snapshot; see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SnapshotActions {
    pub(crate) focus: bool,
    pub(crate) selection: Option<SnapshotSelection>,
    pub(crate) split: Option<SnapshotSplit>,
}

impl SnapshotActions {
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let focus = match lookup(SNAPSHOT_FOCUS_ENV).as_deref() {
            None | Some("" | "0") => false,
            Some("1") => true,
            Some(other) => return Err(format!("{SNAPSHOT_FOCUS_ENV}={other:?} is not 0 or 1")),
        };
        let selection = lookup(SNAPSHOT_SELECTION_ENV)
            .filter(|text| !text.is_empty())
            .map(|text| SnapshotSelection::parse(&text))
            .transpose()?;
        let split = match lookup(SNAPSHOT_SPLIT_ENV).as_deref() {
            None | Some("") => None,
            Some(direction) => {
                let direction = match direction {
                    "right" => SnapshotSplitDirection::Right,
                    "bottom" => SnapshotSplitDirection::Bottom,
                    other => {
                        return Err(format!(
                            "{SNAPSHOT_SPLIT_ENV}={other:?} is not right or bottom"
                        ));
                    }
                };
                let command = lookup(SNAPSHOT_SPLIT_COMMAND_ENV)
                    .filter(|command| !command.is_empty())
                    .ok_or_else(|| {
                        format!("{SNAPSHOT_SPLIT_ENV} needs {SNAPSHOT_SPLIT_COMMAND_ENV}")
                    })?;
                Some(SnapshotSplit { direction, command })
            }
        };
        Ok(Self {
            focus,
            selection,
            split,
        })
    }
}

/// What the window should do for its snapshot request in this paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotStep {
    /// Nothing yet.
    Idle,
    /// Split the active pane (its title became [`SPLIT_TITLE`]).
    Split(SnapshotSplit),
    /// The snapshot title is up: set focus and selection, then paint again.
    Prepare {
        focus: bool,
        selection: Option<SnapshotSelection>,
    },
    /// Draw this paint into the snapshot at the path.
    Take(PathBuf),
}

/// A pending one-shot snapshot request for one window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderSnapshotRequest {
    path: PathBuf,
    title: String,
    taken: bool,
    actions: SnapshotActions,
    prepared: bool,
}

impl RenderSnapshotRequest {
    pub(crate) fn from_env() -> Option<Self> {
        let mut request = Self::from_values(
            std::env::var_os(SNAPSHOT_PATH_ENV),
            std::env::var(SNAPSHOT_TITLE_ENV).ok(),
        )?;
        match SnapshotActions::from_lookup(|name| std::env::var(name).ok()) {
            Ok(actions) => request.actions = actions,
            Err(error) => {
                // A malformed action would snapshot the wrong state; take none.
                log::error!("render snapshot disabled: {error}");
                return None;
            }
        }
        Some(request)
    }

    fn from_values(path: Option<std::ffi::OsString>, title: Option<String>) -> Option<Self> {
        let path = PathBuf::from(path.filter(|path| !path.is_empty())?);
        let title = title
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| DEFAULT_SNAPSHOT_TITLE.to_string());
        Some(Self {
            path,
            title,
            taken: false,
            actions: SnapshotActions::default(),
            prepared: false,
        })
    }

    /// Advances the request for a paint whose active pane has `active_title`.
    pub(crate) fn step(&mut self, active_title: Option<&str>) -> SnapshotStep {
        if self.taken {
            return SnapshotStep::Idle;
        }
        if active_title == Some(SPLIT_TITLE) {
            if let Some(split) = self.actions.split.take() {
                return SnapshotStep::Split(split);
            }
        }
        if active_title != Some(self.title.as_str()) {
            return SnapshotStep::Idle;
        }
        if !self.prepared && (self.actions.focus || self.actions.selection.is_some()) {
            self.prepared = true;
            return SnapshotStep::Prepare {
                focus: self.actions.focus,
                selection: self.actions.selection,
            };
        }
        self.taken = true;
        SnapshotStep::Take(self.path.clone())
    }

    /// Returns a claimed snapshot to pending: the frame was not final yet
    /// (fallback fonts still resolving), so a later paint takes it.
    pub(crate) fn rearm(&mut self) {
        self.taken = false;
    }
}

/// Converts tightly packed texels of `format` into RGBA8. Only the 8-bit
/// four-channel formats a presentable surface uses are accepted.
pub(crate) fn texels_to_rgba8(
    format: wgpu::TextureFormat,
    mut texels: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
    use wgpu::TextureFormat as F;
    match format {
        F::Rgba8Unorm | F::Rgba8UnormSrgb => Ok(texels),
        F::Bgra8Unorm | F::Bgra8UnormSrgb => {
            for pixel in texels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            Ok(texels)
        }
        other => anyhow::bail!("render snapshot cannot read surface format {other:?}"),
    }
}

/// Writes an RGBA8 PNG via a temporary file and a rename, so a reader polling
/// for `path` never sees a partial image.
pub(crate) fn write_png_atomically(
    path: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> anyhow::Result<()> {
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .with_context(|| format!("snapshot buffer does not hold {width}x{height} RGBA8 pixels"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    image
        .save_with_format(&partial, image::ImageFormat::Png)
        .with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, path)
        .with_context(|| format!("renaming {} to {}", partial.display(), path.display()))?;
    log::info!(
        "render snapshot written: {} ({width}x{height})",
        path.display()
    );
    Ok(())
}

/// Where one WebGPU frame is drawn.
pub(crate) enum WebGpuDrawTarget {
    /// The window surface: drawn, submitted and presented.
    Surface(crate::termwindow::webgpu::AcquiredWebGpuFrame),
    /// A snapshot texture: drawn with the same pipeline, read back to a PNG,
    /// never presented.
    Snapshot(SnapshotTarget),
}

/// The offscreen texture one snapshot paint draws into: the surface's format
/// and size, readable (`COPY_SRC`).
pub(crate) struct SnapshotTarget {
    pub(crate) texture: wgpu::Texture,
    pub(crate) path: PathBuf,
}

impl SnapshotTarget {
    pub(crate) fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        path: PathBuf,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            width > 0 && height > 0,
            "cannot snapshot a {width}x{height} surface"
        );
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frankenterm-gui render snapshot target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Ok(Self { texture, path })
    }
}

/// A texture-to-buffer copy recorded into a frame's encoder, read back after
/// that frame is submitted.
pub(crate) struct PendingReadback {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    format: wgpu::TextureFormat,
    path: PathBuf,
}

impl PendingReadback {
    /// Records a copy of `texture` (which must have `COPY_SRC` usage) into
    /// `encoder`, ahead of the frame's submission.
    pub(crate) fn record(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        path: PathBuf,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            texture.usage().contains(wgpu::TextureUsages::COPY_SRC),
            "snapshot texture lacks COPY_SRC usage"
        );
        let size = texture.size();
        let (width, height) = (size.width, size.height);
        let unpadded = width.checked_mul(4).context("snapshot width overflows")?;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded.div_ceil(align) * align;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frankenterm-gui render snapshot readback"),
            size: u64::from(padded_bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            size,
        );
        Ok(Self {
            buffer,
            width,
            height,
            padded_bytes_per_row,
            format: texture.format(),
            path,
        })
    }

    /// Waits for the copy (submitted with the frame), then writes the PNG.
    pub(crate) fn finish(self, device: &wgpu::Device) -> anyhow::Result<()> {
        let slice = self.buffer.slice(..);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        let start = Instant::now();
        loop {
            device
                .poll(wgpu::PollType::Poll)
                .map_err(|err| anyhow::anyhow!("polling the device for the snapshot: {err:?}"))?;
            match receiver.recv_timeout(READBACK_POLL_INTERVAL) {
                Ok(Ok(())) => break,
                Ok(Err(err)) => anyhow::bail!("mapping the snapshot buffer failed: {err:?}"),
                Err(mpsc::RecvTimeoutError::Timeout) if start.elapsed() < READBACK_TIMEOUT => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::bail!("timed out mapping the snapshot buffer")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("snapshot mapping callback was dropped")
                }
            }
        }
        let unpadded = self.width as usize * 4;
        let mut texels = Vec::with_capacity(unpadded * self.height as usize);
        {
            let mapped = slice
                .get_mapped_range()
                .map_err(|err| anyhow::anyhow!("reading the mapped snapshot buffer: {err:?}"))?;
            for row in 0..self.height as usize {
                let start = row * self.padded_bytes_per_row as usize;
                texels.extend_from_slice(&mapped[start..start + unpadded]);
            }
        }
        self.buffer.unmap();
        let rgba = texels_to_rgba8(self.format, texels)?;
        write_png_atomically(&self.path, self.width, self.height, &rgba)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_needs_a_non_empty_path_and_defaults_the_title() {
        assert_eq!(RenderSnapshotRequest::from_values(None, None), None);
        assert_eq!(
            RenderSnapshotRequest::from_values(Some("".into()), None),
            None
        );
        let request =
            RenderSnapshotRequest::from_values(Some("/tmp/a.png".into()), Some(String::new()))
                .expect("request");
        assert_eq!(request.title, DEFAULT_SNAPSHOT_TITLE);
        assert_eq!(request.path, PathBuf::from("/tmp/a.png"));
    }

    fn take(path: &str) -> SnapshotStep {
        SnapshotStep::Take(PathBuf::from(path))
    }

    #[test]
    fn the_snapshot_is_taken_once_and_only_on_the_sentinel_title() {
        let mut request =
            RenderSnapshotRequest::from_values(Some("/tmp/a.png".into()), Some("done".into()))
                .unwrap();
        assert_eq!(request.step(None), SnapshotStep::Idle);
        assert_eq!(request.step(Some("zsh")), SnapshotStep::Idle);
        assert_eq!(request.step(Some("done")), take("/tmp/a.png"));
        assert_eq!(
            request.step(Some("done")),
            SnapshotStep::Idle,
            "a snapshot is taken once"
        );
        request.rearm();
        assert_eq!(
            request.step(Some("done")),
            take("/tmp/a.png"),
            "a re-armed snapshot is claimable again"
        );
    }

    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn actions_parse_from_the_environment() {
        assert_eq!(
            SnapshotActions::from_lookup(lookup(&[])),
            Ok(SnapshotActions::default())
        );
        let actions = SnapshotActions::from_lookup(lookup(&[
            (SNAPSHOT_FOCUS_ENV, "1"),
            (SNAPSHOT_SELECTION_ENV, "1, 2,3,40"),
            (SNAPSHOT_SPLIT_ENV, "bottom"),
            (SNAPSHOT_SPLIT_COMMAND_ENV, "cat b; exec sleep 600"),
        ]))
        .unwrap();
        assert!(actions.focus);
        assert_eq!(
            actions.selection,
            Some(SnapshotSelection {
                start_row: 1,
                start_col: 2,
                end_row: 3,
                end_col: 40,
            })
        );
        assert_eq!(
            actions.split,
            Some(SnapshotSplit {
                direction: SnapshotSplitDirection::Bottom,
                command: "cat b; exec sleep 600".to_string(),
            })
        );
    }

    #[test]
    fn malformed_actions_are_errors() {
        for vars in [
            &[(SNAPSHOT_FOCUS_ENV, "yes")][..],
            &[(SNAPSHOT_SELECTION_ENV, "1,2,3")][..],
            &[(SNAPSHOT_SELECTION_ENV, "1,2,x,4")][..],
            &[(SNAPSHOT_SELECTION_ENV, "3,0,1,0")][..],
            &[
                (SNAPSHOT_SPLIT_ENV, "left"),
                (SNAPSHOT_SPLIT_COMMAND_ENV, "x"),
            ][..],
            &[(SNAPSHOT_SPLIT_ENV, "right")][..],
        ] {
            assert!(
                SnapshotActions::from_lookup(lookup(vars)).is_err(),
                "{vars:?}"
            );
        }
    }

    #[test]
    fn focus_and_selection_are_prepared_one_paint_before_the_snapshot() {
        let mut request =
            RenderSnapshotRequest::from_values(Some("/tmp/a.png".into()), None).unwrap();
        let selection = SnapshotSelection {
            start_row: 0,
            start_col: 0,
            end_row: 1,
            end_col: 5,
        };
        request.actions = SnapshotActions {
            focus: true,
            selection: Some(selection),
            split: None,
        };
        assert_eq!(request.step(Some("zsh")), SnapshotStep::Idle);
        assert_eq!(
            request.step(Some(DEFAULT_SNAPSHOT_TITLE)),
            SnapshotStep::Prepare {
                focus: true,
                selection: Some(selection),
            }
        );
        assert_eq!(
            request.step(Some(DEFAULT_SNAPSHOT_TITLE)),
            take("/tmp/a.png")
        );
        // A re-armed snapshot does not prepare twice.
        request.rearm();
        assert_eq!(
            request.step(Some(DEFAULT_SNAPSHOT_TITLE)),
            take("/tmp/a.png")
        );
    }

    #[test]
    fn a_split_fires_once_on_its_own_title() {
        let mut request =
            RenderSnapshotRequest::from_values(Some("/tmp/a.png".into()), None).unwrap();
        let split = SnapshotSplit {
            direction: SnapshotSplitDirection::Right,
            command: "cat b".to_string(),
        };
        request.actions.split = Some(split.clone());
        assert_eq!(request.step(Some("zsh")), SnapshotStep::Idle);
        assert_eq!(request.step(Some(SPLIT_TITLE)), SnapshotStep::Split(split));
        assert_eq!(request.step(Some(SPLIT_TITLE)), SnapshotStep::Idle);
        assert_eq!(
            request.step(Some(DEFAULT_SNAPSHOT_TITLE)),
            take("/tmp/a.png")
        );
    }

    #[test]
    fn bgra_texels_are_swizzled_and_rgba_texels_kept() {
        let texels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(
            texels_to_rgba8(wgpu::TextureFormat::Bgra8UnormSrgb, texels.clone()).unwrap(),
            vec![3, 2, 1, 4, 7, 6, 5, 8]
        );
        assert_eq!(
            texels_to_rgba8(wgpu::TextureFormat::Rgba8Unorm, texels.clone()).unwrap(),
            texels
        );
        assert!(texels_to_rgba8(wgpu::TextureFormat::Rgba16Float, texels).is_err());
    }

    #[test]
    fn png_is_written_atomically_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/frame.png");
        let rgba: Vec<u8> = (0..2 * 3 * 4).map(|i| i as u8).collect();
        write_png_atomically(&path, 2, 3, &rgba).unwrap();
        assert!(!dir.path().join("nested/frame.png.partial").exists());
        let back = image::open(&path).unwrap().into_rgba8();
        assert_eq!(back.dimensions(), (2, 3));
        assert_eq!(back.into_raw(), rgba);
        assert!(
            write_png_atomically(&path, 5, 5, &rgba).is_err(),
            "size mismatch is refused"
        );
    }
}
