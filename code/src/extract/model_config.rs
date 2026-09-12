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
    #[serde(default)]
    pub fallback: FallbackConfig,
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
    /// Only set on `[[fallback.candidates]]` entries (ml-engineer 2f0746b): "llm", "vlm",
    /// or "llm_and_vlm" for a multimodal backup that covers both roles with one model.
    /// `None` for the primary `[[candidates.vlm]]`/`[[candidates.llm]]` entries, which
    /// already know their role from which array they're in.
    #[serde(default)]
    pub role: Option<String>,
}

/// Backup/fallback model catalog (ml-engineer 2f0746b, user request): frontier-class,
/// more-expensive models tried only when a primary candidate's output fails validation/
/// reconciliation or the primary is unavailable — never a routine first choice. Separate
/// from `candidates` so it can never accidentally become a primary pick.
#[derive(Debug, Default, Deserialize)]
pub struct FallbackConfig {
    #[serde(default)]
    pub candidates: Vec<CandidateConfig>,
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
    /// Candidate id from `[[candidates.vlm]]` for a backup frontier model, tried after
    /// `vlm_escalation` also fails to reconcile (or when `vlm_primary`/`vlm_escalation`
    /// themselves are unavailable — a network/provider error, not just a bad reconcile).
    /// User-requested resilience layer beyond the primary bake-off pick.
    pub vlm_fallback: Option<String>,
    /// Candidate id from `[[candidates.llm]]` for messages the deterministic skeleton
    /// parser does not recognize.
    pub llm_primary: Option<String>,
    /// Candidate id from `[[candidates.llm]]` for a backup frontier model, tried when
    /// `llm_primary`'s call fails or its reply doesn't parse into valid records.
    pub llm_fallback: Option<String>,
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

    /// Resolves `[selected].vlm_fallback` against `[[candidates.vlm]]` first, then the
    /// `[[fallback.candidates]]` catalog (ml-engineer 2f0746b) filtered to a "vlm" or
    /// "llm_and_vlm" role.
    pub fn vlm_fallback(&self) -> Option<&CandidateConfig> {
        let id = self.selected.vlm_fallback.as_deref()?;
        self.find_vlm(id).or_else(|| self.find_fallback(id, "vlm"))
    }

    pub fn llm_primary(&self) -> Option<&CandidateConfig> {
        self.selected.llm_primary.as_deref().and_then(|id| self.find_llm(id))
    }

    /// Resolves `[selected].llm_fallback` against `[[candidates.llm]]` first, then the
    /// `[[fallback.candidates]]` catalog filtered to a "llm" or "llm_and_vlm" role.
    pub fn llm_fallback(&self) -> Option<&CandidateConfig> {
        let id = self.selected.llm_fallback.as_deref()?;
        self.find_llm(id).or_else(|| self.find_fallback(id, "llm"))
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

    fn find_fallback(&self, id: &str, wanted_role: &str) -> Option<&CandidateConfig> {
        self.fallback.candidates.iter().find(|c| {
            c.id == id
                && c.role.as_deref().is_some_and(|r| r == wanted_role || r == "llm_and_vlm")
        })
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
        assert!(cfg.vlm_fallback().is_none());
        assert!(cfg.llm_primary().is_none());
        assert!(cfg.llm_fallback().is_none());
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
            vlm_fallback = "vendor/VlmC"
            llm_primary = "vendor/LlmA"
            llm_fallback = "vendor/LlmB"
            image_max_dim_px = 768
            [[candidates.vlm]]
            id = "vendor/VlmA"
            provider = "p1"
            model_revision = "rev1"
            [[candidates.vlm]]
            id = "vendor/VlmB"
            provider = "p2"
            model_revision = "rev2"
            [[candidates.vlm]]
            id = "vendor/VlmC"
            provider = "p4"
            model_revision = "rev4"
            [[candidates.llm]]
            id = "vendor/LlmA"
            provider = "p3"
            model_revision = "rev3"
            [[candidates.llm]]
            id = "vendor/LlmB"
            provider = "p5"
            model_revision = "rev5"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.vlm_primary().unwrap().id, "vendor/VlmA");
        assert_eq!(cfg.vlm_escalation().unwrap().id, "vendor/VlmB");
        assert_eq!(cfg.vlm_fallback().unwrap().id, "vendor/VlmC");
        assert_eq!(cfg.llm_primary().unwrap().id, "vendor/LlmA");
        assert_eq!(cfg.llm_fallback().unwrap().id, "vendor/LlmB");
        assert_eq!(cfg.image_max_dim_px(), 768);
    }
}
