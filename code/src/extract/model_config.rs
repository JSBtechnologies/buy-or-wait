//! `code/config/models.toml` loader (file owner: ml-engineer; this loader lives in
//! extract/ because resolving which model handles which call is extraction's own
//! orchestration concern). Mirrors ml-engineer's `bakeoff.rs` deserialization shape so the
//! two never drift on the config schema.
//!
//! The `[selected]` table is additive and does not exist in the file yet — ml-engineer/the
//! user fill it in once the bake-off concludes and the user picks (PLAN.md Phase 2d).
//! `#[serde(default)]` means its absence today is not an error: every `resolve_*` call
//! just returns `None` and the model path stays inactive until `[selected]` is added.
//! "The user's choice is only a config change" — no extraction code changes when it lands.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ModelsConfig {
    pub decoding: DecodingConfig,
    pub image_preprocessing: ImagePreprocessingConfig,
    pub candidates: CandidatesConfig,
    #[serde(default)]
    pub selected: Selected,
}

#[derive(Debug, Deserialize)]
pub struct DecodingConfig {
    pub temperature: f64,
    pub seed: i64,
    pub max_tokens_vlm: u32,
    pub max_tokens_llm: u32,
}

#[derive(Debug, Deserialize)]
pub struct ImagePreprocessingConfig {
    #[allow(dead_code)] // kept for schema completeness; production reads `selected.image_max_dim_px` instead
    pub candidate_max_dimensions_px: Vec<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CandidatesConfig {
    pub vlm: Vec<CandidateConfig>,
    pub llm: Vec<CandidateConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CandidateConfig {
    pub id: String,
    pub provider: String,
    pub model_revision: String,
    #[serde(default)]
    pub supports_structured_output: bool,
}

/// Filled in once the bake-off concludes and the user picks (PLAN.md Phase 2d). Absent
/// entirely from the file today — every field then defaults to `None`, keeping the model
/// path inactive rather than erroring.
#[derive(Debug, Default, Deserialize)]
pub struct Selected {
    /// Candidate id from `[[candidates.vlm]]` for the first image read.
    pub vlm_primary: Option<String>,
    /// Candidate id from `[[candidates.vlm]]` for the second read on a reconciliation
    /// failure. May equal `vlm_primary` (same model, second attempt) or name a different
    /// candidate.
    pub vlm_escalation: Option<String>,
    /// Candidate id from `[[candidates.llm]]` for messages the deterministic skeleton
    /// parser does not recognize.
    pub llm_primary: Option<String>,
    /// Image max dimension in px to actually ship with (one of
    /// `image_preprocessing.candidate_max_dimensions_px`, PLAN.md §3 token-efficiency
    /// lever). Defaults to 1024 if unset — the bake-off's resolution sweep should
    /// confirm or override this once results land.
    pub image_max_dim_px: Option<u32>,
}

impl ModelsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn vlm_primary(&self) -> Option<&CandidateConfig> {
        self.selected.vlm_primary.as_deref().and_then(|id| self.find_vlm(id))
    }

    pub fn vlm_escalation(&self) -> Option<&CandidateConfig> {
        self.selected.vlm_escalation.as_deref().and_then(|id| self.find_vlm(id))
    }

    pub fn llm_primary(&self) -> Option<&CandidateConfig> {
        self.selected.llm_primary.as_deref().and_then(|id| self.find_llm(id))
    }

    pub fn image_max_dim_px(&self) -> u32 {
        self.selected.image_max_dim_px.unwrap_or(1024)
    }

    fn find_vlm(&self, id: &str) -> Option<&CandidateConfig> {
        self.candidates.vlm.iter().find(|c| c.id == id)
    }

    fn find_llm(&self, id: &str) -> Option<&CandidateConfig> {
        self.candidates.llm.iter().find(|c| c.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_absent_resolves_to_none_not_an_error() {
        let cfg: ModelsConfig = toml::from_str(
            r#"
            [decoding]
            temperature = 0.0
            seed = 42
            max_tokens_vlm = 400
            max_tokens_llm = 300
            [image_preprocessing]
            candidate_max_dimensions_px = [512, 1024]
            [candidates]
            vlm = []
            llm = []
            "#,
        )
        .unwrap();
        assert!(cfg.vlm_primary().is_none());
        assert!(cfg.vlm_escalation().is_none());
        assert!(cfg.llm_primary().is_none());
        assert_eq!(cfg.image_max_dim_px(), 1024);
    }

    #[test]
    fn selected_present_resolves_the_named_candidate() {
        let cfg: ModelsConfig = toml::from_str(
            r#"
            [decoding]
            temperature = 0.0
            seed = 42
            max_tokens_vlm = 400
            max_tokens_llm = 300
            [image_preprocessing]
            candidate_max_dimensions_px = [512, 1024]
            [selected]
            vlm_primary = "vendor/VlmA"
            vlm_escalation = "vendor/VlmB"
            llm_primary = "vendor/LlmA"
            image_max_dim_px = 768
            [[candidates.vlm]]
            id = "vendor/VlmA"
            provider = "p1"
            model_revision = "rev1"
            [[candidates.vlm]]
            id = "vendor/VlmB"
            provider = "p2"
            model_revision = "rev2"
            [[candidates.llm]]
            id = "vendor/LlmA"
            provider = "p3"
            model_revision = "rev3"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.vlm_primary().unwrap().id, "vendor/VlmA");
        assert_eq!(cfg.vlm_escalation().unwrap().id, "vendor/VlmB");
        assert_eq!(cfg.llm_primary().unwrap().id, "vendor/LlmA");
        assert_eq!(cfg.image_max_dim_px(), 768);
    }
}
