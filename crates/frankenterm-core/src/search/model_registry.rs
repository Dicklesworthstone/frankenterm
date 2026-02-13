//! Model registry — download, cache, and version management for embedding models.
//!
//! Requires the `semantic-search` feature.

use std::collections::HashMap;
use std::path::PathBuf;

/// Information about a registered model.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    pub version: String,
    pub dimension: usize,
    pub size_bytes: u64,
    pub cache_path: Option<PathBuf>,
}

/// Registry for managing embedding model downloads and caching.
pub struct ModelRegistry {
    models: HashMap<String, ModelInfo>,
    cache_dir: PathBuf,
}

impl ModelRegistry {
    /// Create a new model registry with the given cache directory.
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            models: HashMap::new(),
            cache_dir: cache_dir.into(),
        }
    }

    /// Register a model in the registry.
    pub fn register(&mut self, info: ModelInfo) {
        self.models.insert(info.name.clone(), info);
    }

    /// Look up a model by name.
    pub fn get(&self, name: &str) -> Option<&ModelInfo> {
        self.models.get(name)
    }

    /// List all registered models.
    pub fn list(&self) -> Vec<&ModelInfo> {
        self.models.values().collect()
    }

    /// Get the cache directory.
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }
}
