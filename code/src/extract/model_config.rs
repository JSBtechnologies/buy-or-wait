//! `code/config/models.toml` loader (file owner: ml-engineer; this loader lives in
//! extract/ because resolving which model handles which call is extraction's own
//! orchestration concern).
//!
//! Cleanup `cleanup.remove_vlm_anthropic`: the VLM image-read routing (candidates, multi-
//! model agreement/escalation, tiebreak classes, doc-validation bands) was removed along
//! with the HF/Anthropic VLM call path in `extract::images` -- image reads go through the
//! deterministic OCR path (`extract::ocr`/`extract::ocr_parse`/`extract::labels`) instead,
//! which needs no model routing at all. Only the LLM candidate/decoding config the
//! messages/intake extraction path still needs remains here.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ModelsConfig {
    pub decoding: DecodingConfig,
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
    pub max_tokens_llm: u32,
}

#[derive(Debug, Deserialize)]
pub struct CandidatesConfig {
    #[serde(default)]
    pub llm: Vec<CandidateConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CandidateConfig {
    pub id: String,
    pub provider: String,
    pub model_revision: String,
    #[serde(default)]
    pub supports_structured_output: bool,
    /// Only set on `[[fallback.candidates]]` entries (ml-engineer 2f0746b): "llm" for a
    /// backup frontier model. `None` for the primary `[[candidates.llm]]` entries, which
    /// already know their role from which array they're in.
    #[serde(default)]
    pub role: Option<String>,
}

/// Backup/fallback model catalog (ml-engineer 2f0746b, user request): a frontier-class,
/// more-expensive model tried only when the primary LLM candidate's call fails or its reply
/// doesn't parse -- never a routine first choice. Separate from `candidates` so it can never
/// accidentally become a primary pick.
#[derive(Debug, Default, Deserialize)]
pub struct FallbackConfig {
    #[serde(default)]
    pub candidates: Vec<CandidateConfig>,
}

/// Filled in once the user picks a model for message extraction. Absent entirely from the
/// file today -- every field then defaults to `None`, keeping the LLM path inactive rather
/// than erroring.
#[derive(Debug, Default, Deserialize)]
pub struct Selected {
    /// Candidate id from `[[candidates.llm]]` for messages the deterministic skeleton
    /// parser does not recognize.
    pub llm_primary: Option<String>,
    /// Candidate id from `[[candidates.llm]]` for a backup frontier model, tried when
    /// `llm_primary`'s call fails or its reply doesn't parse into valid records.
    pub llm_fallback: Option<String>,
}

impl ModelsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn llm_primary(&self) -> Option<&CandidateConfig> {
        self.selected.llm_primary.as_deref().and_then(|id| self.find_llm(id))
    }

    /// Resolves `[selected].llm_fallback` against `[[candidates.llm]]` first, then the
    /// `[[fallback.candidates]]` catalog filtered to an "llm" role.
    pub fn llm_fallback(&self) -> Option<&CandidateConfig> {
        let id = self.selected.llm_fallback.as_deref()?;
        self.find_llm(id).or_else(|| self.find_fallback(id))
    }

    fn find_llm(&self, id: &str) -> Option<&CandidateConfig> {
        self.candidates.llm.iter().find(|c| c.id == id)
    }

    fn find_fallback(&self, id: &str) -> Option<&CandidateConfig> {
        self.fallback.candidates.iter().find(|c| c.id == id && c.role.as_deref() == Some("llm"))
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
            max_tokens_llm = 300
            [candidates]
            llm = []
            "#,
        )
        .unwrap();
        assert!(cfg.llm_primary().is_none());
        assert!(cfg.llm_fallback().is_none());
    }

    #[test]
    fn selected_present_resolves_the_named_candidate() {
        let cfg: ModelsConfig = toml::from_str(
            r#"
            [decoding]
            temperature = 0.0
            seed = 42
            max_tokens_llm = 300
            [selected]
            llm_primary = "vendor/LlmA"
            llm_fallback = "vendor/LlmB"
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
        assert_eq!(cfg.llm_primary().unwrap().id, "vendor/LlmA");
        assert_eq!(cfg.llm_fallback().unwrap().id, "vendor/LlmB");
    }

    #[test]
    fn llm_fallback_resolves_from_the_fallback_catalog_when_not_a_primary_candidate() {
        let cfg: ModelsConfig = toml::from_str(
            r#"
            [decoding]
            temperature = 0.0
            seed = 42
            max_tokens_llm = 300
            [selected]
            llm_fallback = "vendor/Frontier"
            [candidates]
            llm = []
            [[fallback.candidates]]
            id = "vendor/Frontier"
            provider = "p9"
            model_revision = "rev9"
            role = "llm"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.llm_fallback().unwrap().id, "vendor/Frontier");
    }
}
