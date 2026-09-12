//! Evidence extraction and request intake (owner: extraction).
//!
//! Boundary contract: this module owns the model-facing typed schemas (PLAN.md §2.5,
//! `code/prompts/`) and converts validated records into `crate::engine::ledger`'s own
//! evidence contract (`EvidenceRecord`, `Fact` — owner: engine, commit 81e1127). Nothing
//! here decides cash treatment; that stays entirely inside `engine::ledger`. If a fact this
//! module needs to express has no matching `Fact` variant, it is dropped (never guessed)
//! and raised on bus topic `blocker` (owner: engine) rather than editing `ledger.rs`.

pub mod grounding;
pub mod images;
pub mod intake;
pub mod messages;
pub mod model_config;
pub mod prompts;
pub mod retrieval;

use anyhow::Result;

/// Parse a model's raw text reply as strict JSON, tolerating an accidental ```json fence
/// despite the prompt instructing against one.
pub fn parse_json_reply(text: &str) -> Result<serde_json::Value> {
    let trimmed = text.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(str::trim)
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    serde_json::from_str(unfenced)
        .map_err(|e| anyhow::anyhow!("model reply is not valid JSON: {e}\nraw: {trimmed}"))
}
