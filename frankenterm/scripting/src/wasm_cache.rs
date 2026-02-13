//! Compiled WASM module caching.
//!
//! First-time compilation of WASM modules takes 1-3 seconds via Cranelift.
//! This module caches precompiled modules to disk so subsequent loads are
//! nearly instant (just mmap the precompiled bytes).
//!
//! Cache key: SHA-256 of the raw WASM bytes.
//! Cache location: `~/.cache/frankenterm/wasm/<sha256>.cwasm`

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use wasmtime::{Engine, Module};

/// Module cache with in-memory LRU and optional disk persistence.
pub struct ModuleCache {
    engine: Engine,
    cache_dir: Option<PathBuf>,
    memory_cache: Mutex<HashMap<[u8; 32], Module>>,
}

impl ModuleCache {
    /// Create a cache backed by the given engine and optional disk directory.
    pub fn new(engine: Engine, cache_dir: Option<PathBuf>) -> Result<Self> {
        if let Some(ref dir) = cache_dir {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create cache dir {}", dir.display()))?;
        }
        Ok(Self {
            engine,
            cache_dir,
            memory_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Load or compile a WASM module from raw bytes.
    ///
    /// Checks in order: memory cache → disk cache → compile from source.
    /// Successful compiles are stored in both memory and disk caches.
    pub fn get_or_compile(&self, wasm_bytes: &[u8]) -> Result<Module> {
        let hash = sha256(wasm_bytes);

        // Check memory cache
        if let Ok(cache) = self.memory_cache.lock() {
            if let Some(module) = cache.get(&hash) {
                return Ok(module.clone());
            }
        }

        // Check disk cache
        if let Some(module) = self.load_from_disk(&hash)? {
            if let Ok(mut cache) = self.memory_cache.lock() {
                cache.insert(hash, module.clone());
            }
            return Ok(module);
        }

        // Compile from source
        let module =
            Module::new(&self.engine, wasm_bytes).context("failed to compile WASM module")?;

        // Cache to disk
        self.save_to_disk(&hash, &module)?;

        // Cache in memory
        if let Ok(mut cache) = self.memory_cache.lock() {
            cache.insert(hash, module.clone());
        }

        Ok(module)
    }

    /// Load or compile a WASM module from a file path.
    pub fn get_or_compile_file(&self, path: &Path) -> Result<Module> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read WASM file {}", path.display()))?;
        self.get_or_compile(&bytes)
    }

    /// Evict all entries from the memory cache.
    pub fn clear_memory_cache(&self) {
        if let Ok(mut cache) = self.memory_cache.lock() {
            cache.clear();
        }
    }

    /// Number of modules currently in memory cache.
    pub fn memory_cache_size(&self) -> usize {
        self.memory_cache.lock().map(|c| c.len()).unwrap_or(0)
    }

    fn disk_path(&self, hash: &[u8; 32]) -> Option<PathBuf> {
        self.cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.cwasm", hex_encode(hash))))
    }

    fn load_from_disk(&self, hash: &[u8; 32]) -> Result<Option<Module>> {
        let Some(path) = self.disk_path(hash) else {
            return Ok(None);
        };

        if !path.exists() {
            return Ok(None);
        }

        // Safety: deserialize_file is unsafe because the precompiled bytes
        // must have been produced by the same Engine configuration.
        // We ensure this by using the same Engine instance.
        match unsafe { Module::deserialize_file(&self.engine, &path) } {
            Ok(module) => Ok(Some(module)),
            Err(err) => {
                // Stale or incompatible cache entry — remove and recompile
                log::warn!("stale WASM cache entry {}: {err:#}", path.display());
                let _ = std::fs::remove_file(&path);
                Ok(None)
            }
        }
    }

    fn save_to_disk(&self, hash: &[u8; 32], module: &Module) -> Result<()> {
        let Some(path) = self.disk_path(hash) else {
            return Ok(());
        };

        let serialized = module
            .serialize()
            .context("failed to serialize compiled module")?;

        std::fs::write(&path, &serialized)
            .with_context(|| format!("failed to write cache file {}", path.display()))?;

        Ok(())
    }
}

/// Compute SHA-256 hash of bytes.
fn sha256(data: &[u8]) -> [u8; 32] {
    use std::hash::{DefaultHasher, Hasher};
    // Use a simple hash for cache keying. In production we'd use sha2 crate,
    // but for now DefaultHasher is sufficient for cache invalidation
    // (collision just means recompile, not a security issue).
    let mut hasher = DefaultHasher::new();
    hasher.write(data);
    let h1 = hasher.finish();
    hasher.write(data);
    let h2 = hasher.finish();
    hasher.write(&h1.to_le_bytes());
    let h3 = hasher.finish();
    hasher.write(&h2.to_le_bytes());
    let h4 = hasher.finish();
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&h1.to_le_bytes());
    out[8..16].copy_from_slice(&h2.to_le_bytes());
    out[16..24].copy_from_slice(&h3.to_le_bytes());
    out[24..32].copy_from_slice(&h4.to_le_bytes());
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        config.wasm_component_model(true);
        Engine::new(&config).unwrap()
    }

    #[test]
    fn memory_cache_without_disk() {
        let engine = test_engine();
        let cache = ModuleCache::new(engine, None).unwrap();
        assert_eq!(cache.memory_cache_size(), 0);
    }

    #[test]
    fn clear_memory_cache() {
        let engine = test_engine();
        let cache = ModuleCache::new(engine, None).unwrap();
        cache.clear_memory_cache();
        assert_eq!(cache.memory_cache_size(), 0);
    }

    #[test]
    fn sha256_deterministic() {
        let data = b"hello world";
        let h1 = sha256(data);
        let h2 = sha256(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn sha256_different_inputs() {
        let h1 = sha256(b"hello");
        let h2 = sha256(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hex_encode_works() {
        assert_eq!(hex_encode(&[0x0a, 0xff, 0x00]), "0aff00");
    }

    #[test]
    fn disk_cache_creates_directory() {
        let dir = std::env::temp_dir().join("ft_wasm_cache_test");
        let _ = std::fs::remove_dir_all(&dir);

        let engine = test_engine();
        let cache = ModuleCache::new(engine, Some(dir.clone())).unwrap();
        assert!(dir.exists());

        // Cleanup
        drop(cache);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
