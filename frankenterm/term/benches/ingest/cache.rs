//! On-disk corpus cache: `<dir>/<name>-<size>-<seed>.bin` plus a `.sha256`
//! sidecar recording the content hash and the generator version.
//!
//! A cached corpus is reused only when its length, generator version and
//! SHA-256 all match; anything else is regenerated in place. Files are written
//! to a temporary name and renamed, so concurrent runs never read a torn file.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::corpus::{Corpus, GENERATOR_VERSION};

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheOutcome {
    Reused,
    Generated,
    /// Read from a caller-supplied file (`--from-file`).
    External,
}

impl CacheOutcome {
    pub fn name(self) -> &'static str {
        match self {
            CacheOutcome::Reused => "cache",
            CacheOutcome::Generated => "generated",
            CacheOutcome::External => "file",
        }
    }
}

pub struct LoadedCorpus {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub path: PathBuf,
    pub outcome: CacheOutcome,
}

/// Picks the cache directory: `$FT_CORPUS_DIR`, else `$CARGO_TARGET_DIR/ft-corpus`,
/// else `ft-corpus` beside the target directory the running binary was built
/// into, else the system temporary directory.
pub fn default_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FT_CORPUS_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .or_else(target_dir_of_current_exe)
        .unwrap_or_else(std::env::temp_dir)
        .join("ft-corpus")
}

/// `<target>/<profile>/{examples,deps}/<binary>` -> `<target>`.
fn target_dir_of_current_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.parent()?.parent()?.to_path_buf())
}

pub fn corpus_path(dir: &Path, corpus: Corpus, size: usize, seed: u64) -> PathBuf {
    dir.join(format!("{}-{size}-{seed:016x}.bin", corpus.name()))
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".sha256");
    PathBuf::from(name)
}

fn sidecar_contents(sha256: &str) -> String {
    format!("{sha256} generator-v{GENERATOR_VERSION}\n")
}

pub fn load_or_generate(
    dir: &Path,
    corpus: Corpus,
    size: usize,
    seed: u64,
) -> io::Result<LoadedCorpus> {
    let path = corpus_path(dir, corpus, size, seed);
    if let Some((bytes, sha256)) = read_verified(&path, size)? {
        return Ok(LoadedCorpus {
            bytes,
            sha256,
            path,
            outcome: CacheOutcome::Reused,
        });
    }
    let bytes = corpus.generate(size, seed);
    let sha256 = sha256_hex(&bytes);
    fs::create_dir_all(dir)?;
    write_atomically(&path, &bytes)?;
    write_atomically(&sidecar_path(&path), sidecar_contents(&sha256).as_bytes())?;
    Ok(LoadedCorpus {
        bytes,
        sha256,
        path,
        outcome: CacheOutcome::Generated,
    })
}

pub fn load_file(path: &Path) -> io::Result<LoadedCorpus> {
    let bytes = fs::read(path)?;
    let sha256 = sha256_hex(&bytes);
    Ok(LoadedCorpus {
        bytes,
        sha256,
        path: path.to_path_buf(),
        outcome: CacheOutcome::External,
    })
}

fn read_if_exists(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_verified(path: &Path, size: usize) -> io::Result<Option<(Vec<u8>, String)>> {
    let sidecar = match read_if_exists(&sidecar_path(path))? {
        Some(sidecar) => sidecar,
        None => return Ok(None),
    };
    let sidecar = String::from_utf8_lossy(&sidecar);
    let mut fields = sidecar.split_whitespace();
    let recorded_sha256 = fields.next().unwrap_or_default();
    if fields.next() != Some(format!("generator-v{GENERATOR_VERSION}").as_str()) {
        return Ok(None);
    }
    let bytes = match read_if_exists(path)? {
        Some(bytes) if bytes.len() == size => bytes,
        _ => return Ok(None),
    };
    let sha256 = sha256_hex(&bytes);
    if sha256 != recorded_sha256 {
        return Ok(None);
    }
    Ok(Some((bytes, sha256)))
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}
