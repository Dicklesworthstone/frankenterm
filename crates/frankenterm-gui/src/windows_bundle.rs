//! The Windows GUI's built-in look. The Mac app ships its default config and
//! the Pragmasevka faces in `Contents/Resources`; the Windows package is a set
//! of executables, so `frankenterm-gui.exe` embeds both and writes them once
//! per user under `%LOCALAPPDATA%\FrankenTerm\bundled\<digest>\`. The config
//! crate then searches that directory like the Mac bundle's resources: after
//! every user config, with its `fonts` directory ahead of the system fonts.

use anyhow::Context;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

const DEFAULT_CONFIG: &str = include_str!("../frankenterm.lua");
const FONT_PAYLOAD_ZST: &[u8] = include_bytes!("../../frankenterm/assets/Pragmasevka_NF.zip.zst");
const FONT_FILES: &[&str] = &[
    "pragmasevka-nf-regular.ttf",
    "pragmasevka-nf-bold.ttf",
    "pragmasevka-nf-italic.ttf",
    "pragmasevka-nf-bolditalic.ttf",
];
/// Written last: a directory without it was not finished.
const COMPLETE: &str = ".complete";

/// Unpacks the built-in look (at most once per user and content) and hands
/// its directory to the config crate. Failure only costs the look: the stock
/// defaults still apply, so it is logged rather than fatal.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn install() {
    let Some(local) = dirs_next::data_local_dir() else {
        log::warn!("FrankenTerm's built-in look is unavailable: no local data directory");
        return;
    };
    match unpack(&local.join("FrankenTerm").join("bundled")) {
        Ok(dir) => config::set_bundled_resources_dir(dir),
        Err(err) => log::warn!("FrankenTerm's built-in look is unavailable: {err:#}"),
    }
}

/// The directory for this build's config and fonts under `base`, written if
/// it is not already complete.
fn unpack(base: &Path) -> anyhow::Result<PathBuf> {
    let mut digest = Sha256::new();
    digest.update(DEFAULT_CONFIG.as_bytes());
    digest.update(FONT_PAYLOAD_ZST);
    let name: String = digest.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let dir = base.join(name);
    if dir.join(COMPLETE).is_file() {
        return Ok(dir);
    }
    // Directories appear complete by rename, so only outside changes leave
    // an unfinished one; it is written again.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("clearing {}", dir.display()))?;
    }
    let staging = base.join(format!(".staging-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clearing {}", staging.display()))?;
    }
    write_bundle(&staging)?;
    if let Err(err) = std::fs::rename(&staging, &dir) {
        // Another window may have finished the same directory first.
        let _ = std::fs::remove_dir_all(&staging);
        if !dir.join(COMPLETE).is_file() {
            return Err(err).with_context(|| format!("installing {}", dir.display()));
        }
    }
    Ok(dir)
}

fn write_bundle(dir: &Path) -> anyhow::Result<()> {
    let fonts = dir.join("fonts");
    std::fs::create_dir_all(&fonts).with_context(|| format!("creating {}", fonts.display()))?;
    std::fs::write(dir.join("frankenterm.lua"), DEFAULT_CONFIG)
        .context("writing frankenterm.lua")?;
    let archive =
        zstd::stream::decode_all(FONT_PAYLOAD_ZST).context("decompressing the font payload")?;
    let mut archive = tar::Archive::new(archive.as_slice());
    let mut written = 0;
    for entry in archive.entries().context("reading the font payload")? {
        let mut entry = entry.context("reading the font payload")?;
        let path = entry
            .path()
            .context("reading the font payload")?
            .into_owned();
        // Only the four named faces, at the archive's top level.
        let Some(name) = path.to_str().filter(|name| FONT_FILES.contains(name)) else {
            continue;
        };
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .with_context(|| format!("reading {name}"))?;
        std::fs::write(fonts.join(name), data).with_context(|| format!("writing {name}"))?;
        written += 1;
    }
    anyhow::ensure!(
        written == FONT_FILES.len(),
        "the font payload held {written} of {} faces",
        FONT_FILES.len()
    );
    std::fs::write(dir.join(COMPLETE), b"").context("finishing the bundle")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacks_the_config_and_all_four_faces_once() {
        let base = tempfile::tempdir().unwrap();
        let dir = unpack(base.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("frankenterm.lua")).unwrap(),
            DEFAULT_CONFIG
        );
        for face in FONT_FILES {
            let data = std::fs::read(dir.join("fonts").join(face)).unwrap();
            assert!(data.len() > 1_000_000, "{face} is a whole face");
        }
        assert!(dir.join(COMPLETE).is_file());

        // A finished directory is reused as it is.
        let marker = dir.join("fonts").join(FONT_FILES[0]);
        let before = std::fs::metadata(&marker).unwrap().modified().unwrap();
        assert_eq!(unpack(base.path()).unwrap(), dir);
        assert_eq!(
            std::fs::metadata(&marker).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn an_unfinished_directory_is_written_again() {
        let base = tempfile::tempdir().unwrap();
        let dir = unpack(base.path()).unwrap();
        std::fs::remove_file(dir.join(COMPLETE)).unwrap();
        std::fs::remove_file(dir.join("fonts").join(FONT_FILES[1])).unwrap();
        assert_eq!(unpack(base.path()).unwrap(), dir);
        assert!(dir.join(COMPLETE).is_file());
        assert!(dir.join("fonts").join(FONT_FILES[1]).is_file());
        let leftovers: Vec<_> = std::fs::read_dir(base.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "no staging directory is left behind ({leftovers:?})"
        );
    }
}
