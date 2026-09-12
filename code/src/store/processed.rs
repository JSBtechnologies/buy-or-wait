//! Persisted preprocessing outputs (owner: integrator, PLAN.md §2.11): per-user
//! reconstructed ledgers, detected streams, extracted image figures, typed message records.
//!
//! This module only persists and retrieves whatever serde-serializable value its caller
//! gives it, addressed by a category (e.g. `"ledger"`, `"image_figures"`) and a key (e.g.
//! `user_id` or `image_id`). The domain types themselves belong to the owning module
//! (`engine`, `extract`), keeping this shared plumbing free of a dependency on their shapes.

use std::fs;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::Serialize;

pub struct ProcessedStore {
    root: PathBuf,
}

impl ProcessedStore {
    /// Opens (creating if needed) a processed-data store rooted at `root`, reusing any
    /// prior contents.
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Opens the store with any prior contents wiped, so preprocessing reruns from empty.
    pub fn open_cold(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, category: &str, key: &str) -> PathBuf {
        self.root.join(category).join(format!("{key}.json"))
    }

    /// Loads a persisted value, or `None` if nothing is stored yet for this category/key.
    pub fn load<T: DeserializeOwned>(
        &self,
        category: &str,
        key: &str,
    ) -> anyhow::Result<Option<T>> {
        let path = self.path_for(category, key);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// Persists `value` under this category/key, overwriting any prior value.
    pub fn save<T: Serialize>(&self, category: &str, key: &str, value: &T) -> anyhow::Result<()> {
        let path = self.path_for(category, key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(value)?)?;
        Ok(())
    }
}
