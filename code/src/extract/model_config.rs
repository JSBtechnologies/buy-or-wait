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

use crate::engine::types::Event;

#[derive(Debug, Deserialize)]
pub struct ModelsConfig {
    pub decoding: DecodingConfig,
    pub image_preprocessing: ImagePreprocessingConfig,
    pub candidates: CandidatesConfig,
    #[serde(default)]
    pub selected: Selected,
    #[serde(default)]
    pub fallback: FallbackConfig,
    /// User decision `decision.vlm_setup`: which linked-event class routes to which pair
    /// of VLM readers, for `agreement` mode. Additive `[vlm_routing]` table — absent from
    /// the file today, so `VlmRoutingConfig::default()` (3 built-in classes) applies until
    /// it's added, same pattern as `[selected]`/`[fallback]`.
    #[serde(default)]
    pub vlm_routing: VlmRoutingConfig,
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
    /// User decision `decision.vlm_setup`: `"agreement"` (two independent reads of the
    /// image must select the same amount before it's trusted, tiebroken by the class's own
    /// distinct `tiebreak` reader on disagreement) or `"escalate"` (the original primary ->
    /// escalation -> fallback chain, PLAN.md §2.3, accepting the first reconciling read).
    /// Accuracy directive (analyst #276): unset now defaults to `"agreement"` -- a lone
    /// model's internally-reconciled read is not trustworthy on its own (two same-model
    /// image_02 reads both misread an Indian lakh grouping by 10x and still reconciled), so
    /// `vlm_mode` must be set explicitly to `"escalate"` to opt OUT of agreement mode.
    pub vlm_mode: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VlmMode {
    Agreement,
    Escalate,
}

/// Which pair of VLM readers handles a class of linked event (user decision
/// `decision.vlm_setup`, board:verify.image_agree_preaudit). `readers` names two role
/// keys ("vlm_primary" | "vlm_escalation" | "vlm_fallback"), resolved against `[selected]`
/// via `ModelsConfig::resolve_role`.
#[derive(Debug, Clone, Deserialize)]
pub struct VlmRouteClass {
    pub name: String,
    /// `EventType::as_str()` values ("income", "expense", ...). Empty = matches any.
    #[serde(default)]
    pub event_types: Vec<String>,
    /// `Status::as_str()` values ("settled", "pending", "scheduled", ...). Empty = any.
    #[serde(default)]
    pub statuses: Vec<String>,
    /// Event categories ("salary", "rent", ...). Empty = any.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Exactly two primary readers.
    pub readers: Vec<ReaderSpec>,
    /// The tiebreak reader, called only on disagreement (or a missing/unreconciled primary
    /// read). User decision `decision.tiebreak_distinct` (analyst audit #215/#219): MUST be
    /// a distinct role from both `readers` entries -- a class whose tiebreak is one of its
    /// own primary readers lets that single model "tiebreak" against its own cached answer
    /// and decide alone (the image_05 false-accept: Kimi read 704.05, Kimi-as-its-own-
    /// tiebreak matched it 5/5). Enforced at `ModelsConfig::load()` (hard error), not at
    /// call time, so a misconfigured class can never silently self-tiebreak.
    pub tiebreak: ReaderSpec,
}

/// One reader slot in a route class: a role key plus its OWN resolution/token budget
/// (ml-engineer #204/lead: the bake-off screened different candidates at different
/// resolutions -- 235B@1024, gemma@768 -- and a thinking model like Kimi-K3 needs a much
/// larger `max_tokens` or it returns nothing (reasoning tokens consume the default
/// budget). Using the wrong px for a candidate also changes the §2.11 cache key, silently
/// triggering a brand-new paid call instead of a cache hit.
#[derive(Debug, Clone, Deserialize)]
pub struct ReaderSpec {
    /// "vlm_primary" | "vlm_escalation" | "vlm_fallback", resolved via `resolve_role`.
    pub role: String,
    /// Falls back to `[selected].image_max_dim_px` (global default) when unset.
    #[serde(default)]
    pub max_dim_px: Option<u32>,
    /// Falls back to `[decoding].max_tokens_vlm` when unset.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl ReaderSpec {
    /// A reader that wants the global `[selected].image_max_dim_px` / `[decoding].max_tokens_vlm`
    /// defaults rather than an override -- available for a hand-written `[vlm_routing]` entry.
    #[allow(dead_code)]
    fn new(role: &str) -> ReaderSpec {
        ReaderSpec { role: role.to_string(), max_dim_px: None, max_tokens: None }
    }

    fn with(role: &str, max_dim_px: u32, max_tokens: u32) -> ReaderSpec {
        ReaderSpec {
            role: role.to_string(),
            max_dim_px: Some(max_dim_px),
            max_tokens: Some(max_tokens),
        }
    }
}

/// One resolved reader: role, candidate, and the exact per-call `max_dim_px`/`max_tokens`
/// to use for it (either the reader's own override or the global default).
#[derive(Debug, Clone, Copy)]
pub struct ReaderPick<'a> {
    pub role: &'a str,
    pub candidate: &'a CandidateConfig,
    pub max_dim_px: u32,
    pub max_tokens: u32,
}

#[derive(Debug, Deserialize)]
pub struct VlmRoutingConfig {
    #[serde(default)]
    pub default_class: Option<String>,
    /// Rounding tolerance (currency units) for cross-model agreement/tiebreak matching in
    /// `extract::images`'s `resolve_blank_amount_agreement` -- how close two readers' (or a
    /// reader and the tiebreak's) selected amounts must be to count as a match. Distinct
    /// from `images::ROUNDING_TOLERANCE_2TERM`, which is a fixed accounting-precision
    /// constant for a document's OWN internal arithmetic (subtotal+tax=total); this one
    /// governs how strict cross-model agreement is, so it is user/config tunable but capped:
    /// verifier gate #250 and board decision `decision.accuracy_first` (0 false accepts is a
    /// hard sign-off gate) require it never exceed the documented default of 1.0 -- a looser
    /// tolerance would accept more disagreeing reads as "agreeing". Enforced at
    /// `ModelsConfig::load()` (hard error), defaults to 1.0 when unset.
    #[serde(default)]
    pub tolerance: Option<f64>,
    #[serde(default)]
    pub classes: Vec<VlmRouteClass>,
}

/// Default/maximum cross-model agreement tolerance (verifier gate #250,
/// `decision.accuracy_first`): 0.5 currency units per side, so two independently rounded
/// reads of the same figure can still agree.
const MAX_AGREEMENT_TOLERANCE: f64 = 1.0;

/// Built-in routing (user decision `decision.vlm_setup`): a linked event classifies as
/// deterministic income/payslip, pending-or-scheduled bill, or settled expense/receipt —
/// derived from `event_type`/`status`/`category` only, never the model's own `doc_type`
/// (analyst audit #184/#194: doc_type is free text and unreliable; the event row is not).
/// Default reader pair for every class is `vlm_primary` + `vlm_escalation` (235B + gemma),
/// EXCEPT pending/scheduled bills: pre-audit board:verify.image_agree_preaudit found gemma
/// consistently drops the due-date fields on that document shape (falls back to the
/// pre-cutoff amount even when the later, larger, correct figure is the one that applies —
/// image_05), so that class routes to `vlm_primary` + `vlm_fallback` (235B + Kimi) instead,
/// per the lead's own example of what the routing table exists to express.
impl Default for VlmRoutingConfig {
    fn default() -> Self {
        // Per-reader px matches what ml-engineer's bake-off actually screened each
        // candidate at (235B@1024, gemma@768) -- using a different px changes the §2.11
        // cache key and silently triggers a new paid call instead of a cache hit. Kimi-K3's
        // max_tokens=1500 (vs the decoding-default ~400) is ml-engineer's finding: as a
        // thinking model it returns 0% output at the default budget (reasoning tokens
        // consume it) -- the final answer still needs extracting from any reasoning
        // preamble, handled by `crate::extract::parse_json_reply`.
        VlmRoutingConfig {
            default_class: Some("settled_expense_receipt".to_string()),
            tolerance: None,
            classes: vec![
                VlmRouteClass {
                    name: "income_payslip".to_string(),
                    event_types: vec!["income".to_string()],
                    statuses: vec![],
                    categories: vec![],
                    readers: vec![
                        ReaderSpec::with("vlm_primary", 1024, 400),
                        ReaderSpec::with("vlm_escalation", 768, 400),
                    ],
                    tiebreak: ReaderSpec::with("vlm_fallback", 1024, 1500),
                },
                VlmRouteClass {
                    name: "pending_bill_due_date".to_string(),
                    event_types: vec![],
                    statuses: vec!["pending".to_string(), "scheduled".to_string()],
                    categories: vec![],
                    // User decision decision.tiebreak_distinct (analyst #215/#219): primary
                    // pair is 235B + Kimi-K3 (gemma drops due-date fields on this shape 4/4,
                    // per the earlier pre-audit); tiebreak is gemma instead of Kimi again --
                    // Kimi-as-its-own-tiebreak was the exact image_05 false accept (5/5,
                    // Kimi read 704.05, matched itself). gemma's tiebreak vote only counts
                    // if it also satisfies the resolved cutoff (images.rs).
                    readers: vec![
                        ReaderSpec::with("vlm_primary", 1024, 400),
                        ReaderSpec::with("vlm_fallback", 1024, 1500),
                    ],
                    tiebreak: ReaderSpec::with("vlm_escalation", 768, 400),
                },
                VlmRouteClass {
                    name: "settled_expense_receipt".to_string(),
                    event_types: vec![],
                    statuses: vec!["settled".to_string()],
                    categories: vec![],
                    readers: vec![
                        ReaderSpec::with("vlm_primary", 1024, 400),
                        ReaderSpec::with("vlm_escalation", 768, 400),
                    ],
                    tiebreak: ReaderSpec::with("vlm_fallback", 1024, 1500),
                },
            ],
        }
    }
}


impl ModelsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: ModelsConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate_vlm_routing()
            .with_context(|| format!("validating [vlm_routing] in {}", path.display()))?;
        Ok(cfg)
    }

    /// User decision `decision.tiebreak_distinct` (analyst #215/#219): a class's `tiebreak`
    /// reader must be a genuinely distinct model from both of its primary `readers` — hard
    /// error at load time, not a runtime skip, so a misconfigured class can never silently
    /// self-tiebreak (the image_05 false accept: Kimi read 704.05, Kimi-as-its-own-tiebreak
    /// matched it 5/5, deciding alone). Checked both by role name (structural) and by the
    /// candidate id each role resolves to today (in case two different role keys happen to
    /// point at the same underlying model in `[selected]`) — the latter check is skipped for
    /// a role that doesn't resolve yet (an unset `[selected]` entry is a valid, inactive
    /// config state, not a distinctness violation).
    fn validate_vlm_routing(&self) -> Result<()> {
        if let Some(tolerance) = self.vlm_routing.tolerance {
            if !(0.0..=MAX_AGREEMENT_TOLERANCE).contains(&tolerance) {
                anyhow::bail!(
                    "[vlm_routing].tolerance must be in [0.0, {MAX_AGREEMENT_TOLERANCE}], got {tolerance} -- decision.accuracy_first requires 0 false accepts, and a looser cross-model agreement tolerance accepts more disagreeing reads as a match"
                );
            }
        }
        for class in &self.vlm_routing.classes {
            if class.readers.len() != 2 {
                anyhow::bail!(
                    "[vlm_routing] class '{}' must have exactly two primary readers, found {}",
                    class.name,
                    class.readers.len()
                );
            }
            for reader in &class.readers {
                if reader.role == class.tiebreak.role {
                    anyhow::bail!(
                        "[vlm_routing] class '{}': tiebreak role '{}' duplicates a primary reader role -- decision.tiebreak_distinct requires the tiebreak to be a distinct model from both primary readers",
                        class.name,
                        class.tiebreak.role
                    );
                }
            }
            if let Some(tb_candidate) = self.resolve_role(&class.tiebreak.role) {
                for reader in &class.readers {
                    if let Some(reader_candidate) = self.resolve_role(&reader.role) {
                        if reader_candidate.id == tb_candidate.id {
                            anyhow::bail!(
                                "[vlm_routing] class '{}': tiebreak role '{}' resolves to the same model ('{}') as primary reader role '{}' -- decision.tiebreak_distinct requires a genuinely distinct model",
                                class.name,
                                class.tiebreak.role,
                                tb_candidate.id,
                                reader.role
                            );
                        }
                    }
                }
            }
        }
        Ok(())
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

    /// Cross-model agreement/tiebreak matching tolerance (verifier gate #250): `[vlm_routing]
    /// .tolerance` if set, else `MAX_AGREEMENT_TOLERANCE` (1.0). Validated at `load()` to
    /// never exceed that ceiling.
    pub fn agreement_tolerance(&self) -> f64 {
        self.vlm_routing.tolerance.unwrap_or(MAX_AGREEMENT_TOLERANCE)
    }

    /// USER DIRECTIVE (accuracy is the only focus, analyst #276: two same-model reads of
    /// image_02 both misread an Indian lakh grouping by 10x and still reconciled internally
    /// -- proof that a single model's reconciled read is not enough on its own). Unset (or
    /// unrecognized) now defaults to `Agreement`, not `Escalate`: an unactivated or
    /// half-configured `[selected]` must never silently fall back to a mode that can accept
    /// a lone model's read. `"escalate"` is still available as an explicit, deliberate
    /// opt-out for anyone who wants the single-reader chain back.
    pub fn vlm_mode(&self) -> VlmMode {
        match self.selected.vlm_mode.as_deref() {
            Some("escalate") => VlmMode::Escalate,
            _ => VlmMode::Agreement,
        }
    }

    /// Resolves a routing role key against `[selected]`. Unknown role names resolve to
    /// `None` (never a guessed candidate).
    pub fn resolve_role(&self, role: &str) -> Option<&CandidateConfig> {
        match role {
            "vlm_primary" => self.vlm_primary(),
            "vlm_escalation" => self.vlm_escalation(),
            "vlm_fallback" => self.vlm_fallback(),
            _ => None,
        }
    }

    /// Deterministic route-class name for a linked event, derived only from
    /// `event_type`/`status`/`category` — never the model's own `doc_type` (analyst audit
    /// #184/#194). The first class whose (empty = wildcard) filters all match wins; falls
    /// back to `vlm_routing.default_class`, or "settled_expense_receipt" if that too is
    /// unset.
    pub fn classify_event(&self, event: &Event) -> &str {
        for class in &self.vlm_routing.classes {
            let type_ok = class.event_types.is_empty()
                || class.event_types.iter().any(|t| t == event.event_type.as_str());
            let status_ok =
                class.statuses.is_empty() || class.statuses.iter().any(|s| s == event.status.as_str());
            let category_ok =
                class.categories.is_empty() || class.categories.iter().any(|c| c == &event.category);
            if type_ok && status_ok && category_ok {
                return &class.name;
            }
        }
        self.vlm_routing.default_class.as_deref().unwrap_or("settled_expense_receipt")
    }

    /// The two-reader pair for `vlm_mode() == Agreement`, for this event's route class:
    /// `(role_a, candidate_a, role_b, candidate_b)`. `None` when the class's reader list has
    /// fewer than two entries, or either role doesn't resolve to a configured candidate
    /// (never guesses a reader pair).
    pub fn readers_for(&self, event: &Event) -> Option<(ReaderPick<'_>, ReaderPick<'_>)> {
        let class_name = self.classify_event(event);
        let class = self.vlm_routing.classes.iter().find(|c| c.name == class_name)?;
        let a = class.readers.first()?;
        let b = class.readers.get(1)?;
        Some((self.resolve_pick(a)?, self.resolve_pick(b)?))
    }

    /// This event's route class's tiebreak reader (user decision `decision.tiebreak_distinct`)
    /// — a genuinely distinct model from both of `readers_for`'s primary readers, enforced at
    /// `load()`. `None` when the class isn't found or the tiebreak role doesn't resolve to a
    /// configured candidate (never guesses a reader).
    pub fn tiebreak_for(&self, event: &Event) -> Option<ReaderPick<'_>> {
        let class_name = self.classify_event(event);
        let class = self.vlm_routing.classes.iter().find(|c| c.name == class_name)?;
        self.resolve_pick(&class.tiebreak)
    }

    fn resolve_pick<'a>(&'a self, spec: &'a ReaderSpec) -> Option<ReaderPick<'a>> {
        let candidate = self.resolve_role(&spec.role)?;
        Some(ReaderPick {
            role: &spec.role,
            candidate,
            max_dim_px: spec.max_dim_px.unwrap_or_else(|| self.image_max_dim_px()),
            max_tokens: spec.max_tokens.unwrap_or(self.decoding.max_tokens_vlm),
        })
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

    fn minimal_cfg_with_selected() -> ModelsConfig {
        toml::from_str(
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
            [candidates]
            llm = []
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
            provider = "p3"
            model_revision = "rev3"
            "#,
        )
        .unwrap()
    }

    fn event_with(
        event_type: crate::engine::types::EventType,
        status: crate::engine::types::Status,
        category: &str,
    ) -> Event {
        use crate::engine::types::{Direction, Flexibility};
        Event {
            id: "event_x".into(),
            event_type,
            description: "x".into(),
            category: category.into(),
            direction: Direction::Debit,
            amount: None,
            currency: "INR".into(),
            event_date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            settlement_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
            status,
            linked_event_id: None,
            flexibility: Flexibility::Fixed,
            minimum_allowed_amount: None,
        }
    }

    /// Accuracy directive (analyst #276: two same-model image_02 reads both misread an
    /// Indian lakh grouping by 10x and still reconciled internally -- proof a lone model's
    /// reconciled read is not trustworthy on its own). Unset, or any value other than the
    /// explicit "escalate" opt-out, must resolve to `Agreement`, never the single-reader
    /// `Escalate` chain -- a half-configured `[selected]` must never silently expose a
    /// single-read acceptance path.
    #[test]
    fn vlm_mode_defaults_to_agreement_and_requires_an_explicit_escalate_opt_out() {
        let unset: ModelsConfig = toml::from_str(
            "[decoding]\ntemperature=0.0\nseed=42\nmax_tokens_vlm=1\nmax_tokens_llm=1\n[image_preprocessing]\ncandidate_max_dimensions_px=[1]\n[candidates]\nvlm=[]\nllm=[]\n",
        )
        .unwrap();
        assert_eq!(unset.vlm_mode(), VlmMode::Agreement);

        let explicit_agreement: ModelsConfig = toml::from_str(
            "[decoding]\ntemperature=0.0\nseed=42\nmax_tokens_vlm=1\nmax_tokens_llm=1\n[image_preprocessing]\ncandidate_max_dimensions_px=[1]\n[selected]\nvlm_mode=\"agreement\"\n[candidates]\nvlm=[]\nllm=[]\n",
        )
        .unwrap();
        assert_eq!(explicit_agreement.vlm_mode(), VlmMode::Agreement);

        let explicit_escalate: ModelsConfig = toml::from_str(
            "[decoding]\ntemperature=0.0\nseed=42\nmax_tokens_vlm=1\nmax_tokens_llm=1\n[image_preprocessing]\ncandidate_max_dimensions_px=[1]\n[selected]\nvlm_mode=\"escalate\"\n[candidates]\nvlm=[]\nllm=[]\n",
        )
        .unwrap();
        assert_eq!(explicit_escalate.vlm_mode(), VlmMode::Escalate);
    }

    /// Built-in routing (user decision `decision.vlm_setup`): classification comes only
    /// from event_type/status/category, and the pending/scheduled-bill class routes to
    /// vlm_primary + vlm_fallback specifically (board:verify.image_agree_preaudit: gemma
    /// drops due-date fields on that shape), not the default vlm_primary + vlm_escalation
    /// pair every other class uses.
    #[test]
    fn builtin_routing_classifies_and_pairs_readers_correctly() {
        use crate::engine::types::{EventType, Status};
        let cfg = minimal_cfg_with_selected();

        let payslip = event_with(EventType::Income, Status::Settled, "salary");
        assert_eq!(cfg.classify_event(&payslip), "income_payslip");
        let (a, b) = cfg.readers_for(&payslip).expect("pair resolves");
        assert_eq!((a.role, &a.candidate.id[..]), ("vlm_primary", "vendor/VlmA"));
        assert_eq!((b.role, &b.candidate.id[..]), ("vlm_escalation", "vendor/VlmB"));

        let pending_bill = event_with(EventType::Expense, Status::Pending, "utilities");
        assert_eq!(cfg.classify_event(&pending_bill), "pending_bill_due_date");
        let (a, b) = cfg.readers_for(&pending_bill).expect("pair resolves");
        assert_eq!((a.role, &a.candidate.id[..]), ("vlm_primary", "vendor/VlmA"));
        assert_eq!((b.role, &b.candidate.id[..]), ("vlm_fallback", "vendor/VlmC"));
        assert_eq!(b.max_tokens, 1500); // Kimi-K3 thinking-model budget (ml-engineer #204)

        let scheduled_bill = event_with(EventType::Expense, Status::Scheduled, "healthcare");
        assert_eq!(cfg.classify_event(&scheduled_bill), "pending_bill_due_date");

        let settled = event_with(EventType::Expense, Status::Settled, "groceries");
        assert_eq!(cfg.classify_event(&settled), "settled_expense_receipt");
        let (a, b) = cfg.readers_for(&settled).expect("pair resolves");
        assert_eq!((a.role, b.role), ("vlm_primary", "vlm_escalation"));
        assert_eq!((a.max_dim_px, b.max_dim_px), (1024, 768));
    }

    /// User decision `decision.tiebreak_distinct`: each class's tiebreak reader is distinct
    /// from its own primary pair -- pending/scheduled bills tiebreak with gemma (not Kimi
    /// again, which was the image_05 self-tiebreak false accept), everything else tiebreaks
    /// with Kimi-K3.
    #[test]
    fn builtin_routing_resolves_a_tiebreak_distinct_from_its_own_readers() {
        use crate::engine::types::{EventType, Status};
        let cfg = minimal_cfg_with_selected();

        let payslip = event_with(EventType::Income, Status::Settled, "salary");
        let tb = cfg.tiebreak_for(&payslip).expect("tiebreak resolves");
        assert_eq!(tb.role, "vlm_fallback");

        let pending_bill = event_with(EventType::Expense, Status::Pending, "utilities");
        let (a, b) = cfg.readers_for(&pending_bill).expect("pair resolves");
        let tb = cfg.tiebreak_for(&pending_bill).expect("tiebreak resolves");
        assert_eq!(tb.role, "vlm_escalation");
        assert_ne!(tb.candidate.id, a.candidate.id);
        assert_ne!(tb.candidate.id, b.candidate.id);

        let settled = event_with(EventType::Expense, Status::Settled, "groceries");
        let tb = cfg.tiebreak_for(&settled).expect("tiebreak resolves");
        assert_eq!(tb.role, "vlm_fallback");
        assert_eq!(tb.max_tokens, 1500);
    }

    /// User decision `decision.tiebreak_distinct` (analyst #215/#219): the built-in
    /// `Default` routing must itself pass validation -- it is the fallback that applies
    /// whenever `[vlm_routing]` is absent from `models.toml`, so a broken default would
    /// silently disable the whole image path.
    #[test]
    fn builtin_default_routing_passes_distinctness_validation() {
        let cfg = minimal_cfg_with_selected();
        cfg.validate_vlm_routing().expect("built-in Default routing must be internally distinct");
    }

    /// A `[vlm_routing]` class whose `tiebreak` role names one of its own two primary
    /// `readers` must be a hard load-time error -- this is the exact shape of the image_05
    /// false accept (analyst #214/#219): Kimi read 704.05, then "tiebreak"-called Kimi again,
    /// which trivially matched its own cached answer and decided alone.
    #[test]
    fn tiebreak_duplicating_a_primary_reader_role_is_a_hard_error() {
        let cfg: ModelsConfig = toml::from_str(
            r#"
            [decoding]
            temperature = 0.0
            seed = 42
            max_tokens_vlm = 400
            max_tokens_llm = 300
            [image_preprocessing]
            candidate_max_dimensions_px = [1024]
            [candidates]
            vlm = []
            llm = []
            [[vlm_routing.classes]]
            name = "bad_class"
            [[vlm_routing.classes.readers]]
            role = "vlm_primary"
            [[vlm_routing.classes.readers]]
            role = "vlm_escalation"
            [vlm_routing.classes.tiebreak]
            role = "vlm_primary"
            "#,
        )
        .unwrap();
        let err = cfg.validate_vlm_routing().expect_err("self-tiebreak by role must be rejected");
        assert!(err.to_string().contains("duplicates a primary reader role"), "{err}");
    }

    /// Even when the tiebreak role KEY differs from both reader role keys, if `[selected]`
    /// happens to point two different role keys at the same underlying candidate id, that is
    /// still a self-tiebreak in substance and must be rejected too.
    #[test]
    fn tiebreak_resolving_to_the_same_candidate_id_as_a_reader_is_a_hard_error() {
        let cfg: ModelsConfig = toml::from_str(
            r#"
            [decoding]
            temperature = 0.0
            seed = 42
            max_tokens_vlm = 400
            max_tokens_llm = 300
            [image_preprocessing]
            candidate_max_dimensions_px = [1024]
            [selected]
            vlm_primary = "vendor/Same"
            vlm_escalation = "vendor/Other"
            vlm_fallback = "vendor/Same"
            [candidates]
            llm = []
            [[candidates.vlm]]
            id = "vendor/Same"
            provider = "p1"
            model_revision = "rev1"
            [[candidates.vlm]]
            id = "vendor/Other"
            provider = "p2"
            model_revision = "rev2"
            [[vlm_routing.classes]]
            name = "bad_class"
            [[vlm_routing.classes.readers]]
            role = "vlm_primary"
            [[vlm_routing.classes.readers]]
            role = "vlm_escalation"
            [vlm_routing.classes.tiebreak]
            role = "vlm_fallback"
            "#,
        )
        .unwrap();
        let err = cfg
            .validate_vlm_routing()
            .expect_err("self-tiebreak by resolved candidate id must be rejected");
        assert!(err.to_string().contains("resolves to the same model"), "{err}");
    }

    #[test]
    fn readers_for_is_none_when_a_role_does_not_resolve() {
        use crate::engine::types::{EventType, Status};
        // No [selected] at all -> every role resolves to None.
        let cfg: ModelsConfig = toml::from_str(
            "[decoding]\ntemperature=0.0\nseed=42\nmax_tokens_vlm=1\nmax_tokens_llm=1\n[image_preprocessing]\ncandidate_max_dimensions_px=[1]\n[candidates]\nvlm=[]\nllm=[]\n",
        )
        .unwrap();
        let event = event_with(EventType::Income, Status::Settled, "salary");
        assert!(cfg.readers_for(&event).is_none());
    }
}
