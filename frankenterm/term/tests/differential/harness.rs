//! Lockstep comparison, minimization, reproducers and campaigns.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Braced imports never mix naming cases: edition 2018 and 2024 rustfmt sort
// those differently, and this module is formatted under both.
use super::corpus::Rng;
use super::ddmin::ddmin;
use super::engine::{self, EngineFactory, Geometry, Legacy};
use super::snapshot::describe_difference;
use super::streams::{chunk_with_seed, random_stream};

/// `fails` calls each minimization phase may spend.
pub const MINIMIZE_BUDGET: usize = 4000;

#[derive(Clone, Debug, PartialEq)]
pub struct Divergence {
    pub candidate: &'static str,
    /// Index of the chunk after which the snapshots first differed.
    pub chunk_index: usize,
    pub description: String,
}

/// Feeds `chunks` to fresh oracle and candidate engines in lockstep and
/// compares their snapshots after every chunk.
pub fn run_lockstep(
    oracle: &dyn EngineFactory,
    candidate: &dyn EngineFactory,
    geometry: &Geometry,
    chunks: &[Vec<u8>],
) -> Option<Divergence> {
    let mut expected = oracle.build(geometry);
    let mut actual = candidate.build(geometry);
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        expected.feed(chunk);
        actual.feed(chunk);
        if let Some(description) = describe_difference(&expected.snapshot(), &actual.snapshot()) {
            return Some(Divergence {
                candidate: candidate.name(),
                chunk_index,
                description,
            });
        }
    }
    None
}

fn regroup(units: &[(usize, u8)]) -> Vec<Vec<u8>> {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut current_id = None;
    for &(id, byte) in units {
        if current_id == Some(id) {
            chunks.last_mut().expect("a chunk is open").push(byte);
        } else {
            chunks.push(vec![byte]);
            current_id = Some(id);
        }
    }
    chunks
}

/// Shrinks a diverging chunk list: ddmin over whole chunks, then ddmin over
/// single bytes with the surviving chunk boundaries kept. The result still
/// diverges.
pub fn minimize(
    oracle: &dyn EngineFactory,
    candidate: &dyn EngineFactory,
    geometry: &Geometry,
    chunks: Vec<Vec<u8>>,
) -> Vec<Vec<u8>> {
    let chunks = ddmin(chunks, MINIMIZE_BUDGET, &mut |list: &[Vec<u8>]| {
        run_lockstep(oracle, candidate, geometry, list).is_some()
    });
    let units: Vec<(usize, u8)> = chunks
        .iter()
        .enumerate()
        .flat_map(|(id, chunk)| chunk.iter().map(move |&byte| (id, byte)))
        .collect();
    let units = ddmin(units, MINIMIZE_BUDGET, &mut |units: &[(usize, u8)]| {
        run_lockstep(oracle, candidate, geometry, &regroup(units)).is_some()
    });
    regroup(&units)
}

/// A minimized divergence, replayable from disk.
#[derive(Clone, Debug, PartialEq)]
pub struct Repro {
    pub candidate: String,
    pub geometry: Geometry,
    pub chunks: Vec<Vec<u8>>,
    pub description: String,
}

const REPRO_MAGIC: &str = "ft-engine-differential-repro v1";

impl Repro {
    /// Header lines, a blank line, then the concatenated chunk bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let lengths: Vec<String> = self
            .chunks
            .iter()
            .map(|chunk| chunk.len().to_string())
            .collect();
        let mut out = format!(
            "{}\ncandidate={}\ngeometry={},{},{}\nchunks={}\n\n",
            REPRO_MAGIC,
            self.candidate,
            self.geometry.rows,
            self.geometry.cols,
            self.geometry.scrollback,
            lengths.join(",")
        )
        .into_bytes();
        for chunk in &self.chunks {
            out.extend_from_slice(chunk);
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Repro, String> {
        let split = bytes
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .ok_or("no header terminator")?;
        let header = std::str::from_utf8(&bytes[..split]).map_err(|e| e.to_string())?;
        let mut body = &bytes[split + 2..];
        let mut lines = header.lines();
        if lines.next() != Some(REPRO_MAGIC) {
            return Err("not an engine-differential reproducer".to_string());
        }
        let mut candidate = None;
        let mut geometry = None;
        let mut lengths = None;
        for line in lines {
            let (key, value) = line.split_once('=').ok_or("malformed header line")?;
            match key {
                "candidate" => candidate = Some(value.to_string()),
                "geometry" => {
                    let parts: Vec<usize> = value
                        .split(',')
                        .map(|part| part.parse::<usize>().map_err(|e| e.to_string()))
                        .collect::<Result<_, _>>()?;
                    if parts.len() != 3 {
                        return Err("geometry needs rows,cols,scrollback".to_string());
                    }
                    geometry = Some(Geometry {
                        rows: parts[0],
                        cols: parts[1],
                        scrollback: parts[2],
                    });
                }
                "chunks" => {
                    let parsed: Vec<usize> = if value.is_empty() {
                        Vec::new()
                    } else {
                        value
                            .split(',')
                            .map(|part| part.parse::<usize>().map_err(|e| e.to_string()))
                            .collect::<Result<_, _>>()?
                    };
                    lengths = Some(parsed);
                }
                _ => return Err(format!("unknown header key {:?}", key)),
            }
        }
        let lengths = lengths.ok_or("missing chunks")?;
        let mut chunks = Vec::with_capacity(lengths.len());
        for length in lengths {
            if body.len() < length {
                return Err("body shorter than the chunk lengths".to_string());
            }
            let (head, tail) = body.split_at(length);
            chunks.push(head.to_vec());
            body = tail;
        }
        if !body.is_empty() {
            return Err("body longer than the chunk lengths".to_string());
        }
        Ok(Repro {
            candidate: candidate.ok_or("missing candidate")?,
            geometry: geometry.ok_or("missing geometry")?,
            chunks,
            description: String::new(),
        })
    }

    /// Writes `<stem>.repro` and `<stem>.diff.txt` into `dir`.
    pub fn write(&self, dir: &Path, stem: &str) -> std::io::Result<(PathBuf, PathBuf)> {
        std::fs::create_dir_all(dir)?;
        let repro_path = dir.join(format!("{}.repro", stem));
        let diff_path = dir.join(format!("{}.diff.txt", stem));
        std::fs::write(&repro_path, self.to_bytes())?;
        let mut text = format!(
            "candidate {} diverges from legacy at {}x{} (scrollback {})\n\nchunks:\n",
            self.candidate, self.geometry.rows, self.geometry.cols, self.geometry.scrollback
        );
        for (index, chunk) in self.chunks.iter().enumerate() {
            text.push_str(&format!("  [{}] \"{}\"\n", index, chunk.escape_ascii()));
        }
        text.push_str("\ndifference after the last chunk fed:\n");
        text.push_str(&self.description);
        std::fs::write(&diff_path, text)?;
        Ok((repro_path, diff_path))
    }
}

/// `$FT_DIFFERENTIAL_OUT`, else a directory under the test temporary
/// directory (or the system one outside tests).
pub fn output_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FT_DIFFERENTIAL_OUT") {
        return PathBuf::from(dir);
    }
    option_env!("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ft-engine-differential")
}

fn file_stem(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Runs every candidate against the oracle. On a divergence, minimizes it,
/// writes a reproducer and returns a report naming the files.
pub fn check(name: &str, geometry: &Geometry, chunks: &[Vec<u8>]) -> Result<(), String> {
    for candidate in engine::candidates() {
        let candidate = candidate.as_ref();
        if run_lockstep(&Legacy, candidate, geometry, chunks).is_none() {
            continue;
        }
        let minimized = minimize(&Legacy, candidate, geometry, chunks.to_vec());
        let divergence = run_lockstep(&Legacy, candidate, geometry, &minimized)
            .expect("minimization keeps the divergence");
        let repro = Repro {
            candidate: candidate.name().to_string(),
            geometry: *geometry,
            chunks: minimized,
            description: divergence.description.clone(),
        };
        let stem = file_stem(&format!("{}-{}", name, candidate.name()));
        let written = match repro.write(&output_dir(), &stem) {
            Ok((repro_path, diff_path)) => {
                format!("{} and {}", repro_path.display(), diff_path.display())
            }
            Err(error) => format!("(could not write the reproducer: {})", error),
        };
        return Err(format!(
            "{}: candidate {} diverges from legacy at {}x{}; minimized to {} chunk(s), files {}\n{}",
            name,
            candidate.name(),
            geometry.rows,
            geometry.cols,
            repro.chunks.len(),
            written,
            divergence.description
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Campaign {
    pub cases: usize,
    pub bytes: usize,
}

/// Seeded random streams from `first_seed` upward, each with a geometry and
/// chunk plan derived from its seed, until `max_cases` or `budget` runs out.
pub fn run_campaign(
    first_seed: u64,
    max_cases: usize,
    budget: Duration,
) -> Result<Campaign, String> {
    let start = Instant::now();
    let mut campaign = Campaign { cases: 0, bytes: 0 };
    let mut seed = first_seed;
    while campaign.cases < max_cases && start.elapsed() < budget {
        let mut rng = Rng::new(seed);
        let pieces = rng.range_inclusive(16, 96);
        let stream = random_stream(&mut rng, pieces);
        let geometry = engine::GEOMETRIES[(seed % engine::GEOMETRIES.len() as u64) as usize];
        let chunks = chunk_with_seed(&stream, seed);
        check(&format!("campaign-seed-{}", seed), &geometry, &chunks)?;
        campaign.cases += 1;
        campaign.bytes += stream.len();
        seed = seed.wrapping_add(1);
    }
    Ok(campaign)
}
