//! Disk cache for raw model-call output (owner: integrator, PLAN.md §2.11).
//!
//! Keyed by `sha256(content_hash(input) + model_id + model_revision + prompt_version)`.
//! Stores each model's raw response text; callers own parsing it into their typed schema,
//! so this module has no dependency on `extract`'s or `hf`'s concrete types.

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// Content-addressed key for a single model call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey(String);

impl CacheKey {
    pub fn new(input: &[u8], model_id: &str, model_revision: &str, prompt_version: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(input);
        hasher.update(b"\0");
        hasher.update(model_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(model_revision.as_bytes());
        hasher.update(b"\0");
        hasher.update(prompt_version.as_bytes());
        Self(format!("{:x}", hasher.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Hit/miss counters for the usage report's cache-hit-rate metric (PLAN.md §3 Caching).
#[derive(Debug, Default, Clone, Copy)]
pub struct CacheStats {
    pub hits: usize,
    pub misses: usize,
}

/// A disk cache rooted at one directory, one file per key.
pub struct DiskCache {
    root: PathBuf,
    hits: Cell<usize>,
    misses: Cell<usize>,
}

impl DiskCache {
    /// Opens (creating if needed) a disk cache rooted at `root`, reusing any prior contents.
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            hits: Cell::new(0),
            misses: Cell::new(0),
        })
    }

    /// Opens a disk cache with any prior contents wiped, so the run starts from empty.
    /// This is what the final submission run (`--cold`) uses, per PLAN.md §2.11/§3, so the
    /// usage report reflects real calls rather than cache hits.
    pub fn open_cold(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            hits: Cell::new(0),
            misses: Cell::new(0),
        })
    }

    fn path_for(&self, key: &CacheKey) -> PathBuf {
        self.root.join(format!("{}.json", key.as_str()))
    }

    /// Returns the cached raw response for `key`, if present.
    pub fn get(&self, key: &CacheKey) -> anyhow::Result<Option<String>> {
        let path = self.path_for(key);
        if path.exists() {
            self.hits.set(self.hits.get() + 1);
            Ok(Some(fs::read_to_string(path)?))
        } else {
            self.misses.set(self.misses.get() + 1);
            Ok(None)
        }
    }

    /// Stores the raw response text for `key`.
    pub fn put(&self, key: &CacheKey, raw_response: &str) -> anyhow::Result<()> {
        fs::write(self.path_for(key), raw_response)?;
        Ok(())
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.get(),
            misses: self.misses.get(),
        }
    }
}
