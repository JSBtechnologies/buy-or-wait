//! Model bake-off harness (owner: ml-engineer). PLAN.md §3 / Phase 2d.
//!
//! Runs every VLM candidate against `docs/gold_subset.json` images (labeled +
//! unlabeled counts come from the file itself), sweeping image resolution to find the smallest size that
//! keeps gold accuracy, then runs the chosen resolution 5x for a stability
//! rate. Runs every LLM candidate against the gold messages (all labeled)
//! 5x. Reports valid-JSON rate, field accuracy vs gold, cross-model
//! agreement (VLM, unlabeled items), stability rate, avg tokens/item,
//! latency, and cost — then writes the results into `docs/bakeoff.md`.
//!
//! Does not pick a model. Evidence only (PLAN.md Phase 2d: the user picks).
//!
//! Usage: `cargo run --release --bin bakeoff -- [--runs 5] [--config
//! config/models.toml] [--gold-subset ../docs/gold_subset.json]
//! [--prompts-dir prompts] [--dataset-dir ../dataset] [--out
//! ../docs/bakeoff.md] [--cache-dir store/bakeoff_cache]`
//!
//! `--gold-subset`/`--prompts-dir` may point into another worktree
//! (e.g. `../../extraction/docs/gold_subset.json`) before those files land
//! on `main`.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use buyorwait::hf::{ContentPart, HfClient, ModelCall, Usage};
use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// config/models.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ModelsConfig {
    decoding: DecodingConfig,
    // Kept for schema completeness (production's HfClient reads this section
    // for its own retry policy) — the bake-off deliberately uses its own
    // tighter, hardcoded policy instead (see `main`), so this field itself
    // is unused here.
    #[allow(dead_code)]
    retry: RetryConfig,
    image_preprocessing: ImagePreprocessingConfig,
    candidates: CandidatesConfig,
}

#[derive(Debug, Deserialize)]
struct DecodingConfig {
    temperature: f64,
    seed: i64,
    max_tokens_vlm: u32,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct RetryConfig {
    max_attempts: u32,
    backoff_base_ms: u64,
    backoff_multiplier: f64,
    backoff_max_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ImagePreprocessingConfig {
    candidate_max_dimensions_px: Vec<u32>,
}

#[derive(Debug, Deserialize)]
struct CandidatesConfig {
    vlm: Vec<CandidateConfig>,
    llm: Vec<CandidateConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct CandidateConfig {
    id: String,
    provider: String,
    model_revision: String,
    pricing_usd_per_m_tokens: Pricing,
    #[serde(default)]
    supports_structured_output: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct Pricing {
    input: f64,
    output: f64,
}

impl Pricing {
    fn cost_usd(&self, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        (prompt_tokens as f64 / 1_000_000.0) * self.input
            + (completion_tokens as f64 / 1_000_000.0) * self.output
    }
}

// ---------------------------------------------------------------------------
// docs/gold_subset.json
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GoldSubset {
    images: Vec<GoldImage>,
    messages: Vec<GoldMessage>,
}

#[derive(Debug, Deserialize)]
struct GoldImage {
    image_id: String,
    #[serde(default)]
    labeled: bool,
    #[serde(default)]
    expected_figures: Option<Value>,
    /// The event context this image is linked to — only `currency` is used
    /// here, for the reconciliation check.
    #[serde(default)]
    event_context: Option<Value>,
    /// The one field the deterministic selector (image_transcription.v1.md)
    /// would read for this event, and the correct amount at that field —
    /// gold already encodes the selector's decision, so scoring "selected-
    /// figure accuracy" is just: does the model's value at this field match?
    #[serde(default)]
    expected_selected_field: Option<String>,
    #[serde(default)]
    expected_selected_amount: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct GoldMessage {
    message_id: String,
    text: String,
    expected: Value,
}

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

struct Args {
    config: PathBuf,
    gold_subset: PathBuf,
    prompts_dir: PathBuf,
    dataset_dir: PathBuf,
    out: PathBuf,
    cache_dir: PathBuf,
    runs: u32,
}

fn parse_args() -> Args {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut it = env::args().skip(1);
    while let Some(flag) = it.next() {
        if let Some(key) = flag.strip_prefix("--") {
            if let Some(val) = it.next() {
                map.insert(key.to_string(), val);
            }
        }
    }
    let get = |k: &str, default: &str| -> String { map.get(k).cloned().unwrap_or_else(|| default.to_string()) };
    Args {
        config: PathBuf::from(get("config", "config/models.toml")),
        gold_subset: PathBuf::from(get("gold-subset", "../docs/gold_subset.json")),
        prompts_dir: PathBuf::from(get("prompts-dir", "prompts")),
        dataset_dir: PathBuf::from(get("dataset-dir", "../dataset")),
        out: PathBuf::from(get("out", "../docs/bakeoff.md")),
        cache_dir: PathBuf::from(get("cache-dir", "store/bakeoff_cache")),
        runs: get("runs", "5").parse().unwrap_or(5),
    }
}

// ---------------------------------------------------------------------------
// Prompt file parsing: pull the versioned system prompt / user template out
// of the markdown file. Prompts are never hardcoded in Rust (PLAN.md §3).
// ---------------------------------------------------------------------------

fn parse_prompt_version(markdown: &str) -> Result<String> {
    for line in markdown.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            return Ok(rest.trim().to_string());
        }
    }
    bail!("no `# prompt_version` heading found")
}

/// First fenced code block under the first heading whose text contains `needle`.
fn extract_fenced_block(markdown: &str, needle: &str) -> Result<String> {
    let mut found_heading = false;
    let mut in_fence = false;
    let mut captured = String::new();
    for line in markdown.lines() {
        if !found_heading {
            if line.starts_with('#') && line.contains(needle) {
                found_heading = true;
            }
            continue;
        }
        if !in_fence {
            if line.trim_start().starts_with("```") {
                in_fence = true;
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            break;
        }
        captured.push_str(line);
        captured.push('\n');
    }
    if captured.trim().is_empty() {
        bail!("no fenced block found under a heading containing {needle:?}");
    }
    Ok(captured.trim_end().to_string())
}

struct PromptSet {
    version: String,
    system_prompt: String,
    user_template: String,
}

fn load_prompt(path: &Path, user_template_needle: &str) -> Result<PromptSet> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(PromptSet {
        version: parse_prompt_version(&text)?,
        system_prompt: extract_fenced_block(&text, "System prompt")?,
        user_template: extract_fenced_block(&text, user_template_needle)?,
    })
}

// ---------------------------------------------------------------------------
// JSON parsing of model output (models sometimes wrap in ```json fences
// despite instructions not to; strip that before parsing)
// ---------------------------------------------------------------------------

fn parse_json_loose(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    let cleaned = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(cleaned).ok()
}

// ---------------------------------------------------------------------------
// Field-accuracy scoring (flat-object, numeric-tolerant)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct FieldAccuracy {
    matched: u64,
    total: u64,
}

impl FieldAccuracy {
    fn rate(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.matched as f64 / self.total as f64
        }
    }
    fn add(&mut self, other: FieldAccuracy) {
        self.matched += other.matched;
        self.total += other.total;
    }
}

fn values_match(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (xf, yf) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
            (xf - yf).abs() < 0.01
        }
        _ => a == b,
    }
}

fn compare_flat_object(expected: &Value, actual: Option<&Value>) -> FieldAccuracy {
    let mut acc = FieldAccuracy::default();
    let Some(obj) = expected.as_object() else {
        return acc;
    };
    for (k, exp_v) in obj {
        acc.total += 1;
        if let Some(act_v) = actual.and_then(|v| v.as_object()).and_then(|o| o.get(k)) {
            if values_match(exp_v, act_v) {
                acc.matched += 1;
            }
        }
    }
    acc
}

fn compare_message_records(expected: &Value, actual_records: Option<&Vec<Value>>) -> FieldAccuracy {
    let mut acc = FieldAccuracy::default();
    let mut expected_list: Vec<Value> = expected.as_array().cloned().unwrap_or_default();
    let mut actual_list: Vec<Value> = actual_records.cloned().unwrap_or_default();
    let key = |r: &Value| r.get("record_type").and_then(|v| v.as_str()).unwrap_or("").to_string();
    expected_list.sort_by_key(key);
    actual_list.sort_by_key(key);
    for i in 0..expected_list.len() {
        acc.add(compare_flat_object(&expected_list[i], actual_list.get(i)));
    }
    acc
}

// ---------------------------------------------------------------------------
// Selected-figure accuracy + reconciliation (lead directive: the one amount
// the engine would actually use matters more than raw all-field accuracy).
// Gold already encodes which field the deterministic selector
// (image_transcription.v1.md) would read for each labeled image, so scoring
// this is just: does the model's value at that field match?
// ---------------------------------------------------------------------------

/// `None` when no reconciliation check was applicable (not enough of the
/// relevant fields were non-null to check anything); `Some(true/false)`
/// otherwise. Mirrors image_transcription.v1.md's reconciliation rules.
fn reconciliation_ok(figures: &Value, event_currency: Option<&str>) -> Option<bool> {
    let getf = |k: &str| figures.get(k).and_then(|v| v.as_f64());
    const TOL: f64 = 0.01;
    let mut any_check = false;
    let mut all_pass = true;
    let mut check = |applicable: bool, ok: bool| {
        if applicable {
            any_check = true;
            if !ok {
                all_pass = false;
            }
        }
    };
    if let (Some(subtotal), Some(tax), Some(total)) = (getf("subtotal"), getf("tax"), getf("total")) {
        check(true, (subtotal + tax - total).abs() <= TOL);
    }
    if let (Some(gross), Some(ded), Some(net)) = (getf("gross_pay"), getf("deductions"), getf("net_pay")) {
        check(true, (gross - ded - net).abs() <= TOL);
    }
    if let (Some(paid), Some(bal), Some(total)) = (getf("amount_paid"), getf("balance_due"), getf("total")) {
        check(true, (paid + bal - total).abs() <= TOL);
    }
    if let Some(sum_check) = getf("line_items_sum_check") {
        if let Some(subtotal) = getf("subtotal") {
            check(true, (sum_check - subtotal).abs() <= TOL);
        } else if let Some(total) = getf("total") {
            check(true, (sum_check - total).abs() <= TOL);
        }
    }
    if let Some(ec) = event_currency {
        if let Some(cur) = figures.get("currency").and_then(|v| v.as_str()) {
            check(true, cur.eq_ignore_ascii_case(ec));
        }
    }
    any_check.then_some(all_pass)
}

#[derive(Debug, Default, Clone)]
struct SelectorStats {
    /// Labeled-image runs where gold specifies an expected selected field
    /// and the model produced a JSON figure object to check it against.
    checked: u32,
    selected_correct: u32,
    reconciliation_checked: u32,
    reconciliation_passed: u32,
    /// Among wrong selected-figure reads, how many were caught by a failed
    /// reconciliation check (i.e. the safety net actually would have fired).
    wrong_reads: u32,
    wrong_reads_caught_by_reconciliation: u32,
}

impl SelectorStats {
    fn record(&mut self, img: &GoldImage, parsed: Option<&Value>) {
        let (Some(field), Some(expected_amount)) = (&img.expected_selected_field, img.expected_selected_amount)
        else {
            return;
        };
        let Some(figures) = parsed else { return };
        self.checked += 1;
        let actual_amount = figures.get(field).and_then(|v| v.as_f64());
        let correct = actual_amount.map(|a| (a - expected_amount).abs() <= 0.01).unwrap_or(false);
        if correct {
            self.selected_correct += 1;
        } else {
            self.wrong_reads += 1;
        }
        let event_currency = img
            .event_context
            .as_ref()
            .and_then(|ec| ec.get("currency"))
            .and_then(|v| v.as_str());
        if let Some(recon_ok) = reconciliation_ok(figures, event_currency) {
            self.reconciliation_checked += 1;
            if recon_ok {
                self.reconciliation_passed += 1;
            } else if !correct {
                self.wrong_reads_caught_by_reconciliation += 1;
            }
        }
    }

    fn selected_figure_accuracy(&self) -> f64 {
        if self.checked == 0 {
            0.0
        } else {
            self.selected_correct as f64 / self.checked as f64
        }
    }
    fn reconciliation_pass_rate(&self) -> f64 {
        if self.reconciliation_checked == 0 {
            0.0
        } else {
            self.reconciliation_passed as f64 / self.reconciliation_checked as f64
        }
    }
    fn reconciliation_catch_rate(&self) -> f64 {
        if self.wrong_reads == 0 {
            0.0
        } else {
            self.wrong_reads_caught_by_reconciliation as f64 / self.wrong_reads as f64
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregated stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct RunStats {
    calls: u32,
    failed_calls: u32,
    valid_json: u32,
    prompt_tokens: u64,
    completion_tokens: u64,
    latencies_ms: Vec<u64>,
    field_acc: FieldAccuracy,
}

impl RunStats {
    fn record_usage(&mut self, usage: &Usage) {
        self.calls += 1;
        self.prompt_tokens += usage.prompt_tokens;
        self.completion_tokens += usage.completion_tokens;
        self.latencies_ms.push(usage.latency_ms);
    }
    fn record_failure(&mut self) {
        self.calls += 1;
        self.failed_calls += 1;
    }
    fn avg_prompt_tokens(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.prompt_tokens as f64 / self.calls as f64
        }
    }
    fn avg_completion_tokens(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.completion_tokens as f64 / self.calls as f64
        }
    }
    fn p50_latency_ms(&self) -> f64 {
        if self.latencies_ms.is_empty() {
            return 0.0;
        }
        let mut sorted = self.latencies_ms.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2] as f64
    }
    fn valid_json_rate(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.valid_json as f64 / self.calls as f64
        }
    }
}

struct VlmCandidateReport {
    id: String,
    provider: String,
    chosen_max_dim: u32,
    resolution_sweep: Vec<(u32, f64)>, // (max_dim, field accuracy on labeled images)
    stats: RunStats,
    stability_rate: f64,
    pricing: Pricing,
    unavailable: bool,
    selector: SelectorStats,
}

struct LlmCandidateReport {
    id: String,
    provider: String,
    stats: RunStats,
    stability_rate: f64,
    unavailable: bool,
    cost_usd: f64,
    pricing: Pricing,
}

/// The real submission needs exactly one VLM call per blank-amount event
/// (PLAN.md §2.3): 16 images, matching the gold subset's image count exactly.
const FULL_RUN_IMAGES: usize = 16;

/// `messages.csv` has 215 rows, but extraction's template induction resolves
/// most of them deterministically once their skeleton is known (PLAN.md §3
/// batching lever) — the model is only called for *unseen* skeletons. The
/// gold subset (47 messages, hand-picked to cover every record type) contains
/// 13 distinct `record_type` shapes (including combined types); that is a
/// concrete lower-bound proxy for the number of skeletons the full dataset
/// needs resolved by a model call, not a promise of the exact count (final
/// count is extraction's template-induction survey, not ml-engineer's).
const FULL_RUN_MESSAGE_SKELETONS_ESTIMATE: usize = 13;

// ---------------------------------------------------------------------------
// VLM: resolution sweep (1 run/image on the 5 labeled images) then the full
// stability + metrics pass (N runs over all 16 images) at the chosen size.
// ---------------------------------------------------------------------------

fn downscale_and_encode(path: &Path, max_dim: u32) -> Result<String> {
    let img = image::open(path).with_context(|| format!("opening {}", path.display()))?;
    let resized = img.thumbnail(max_dim, max_dim);
    let mut buf = Vec::new();
    resized
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .context("encoding downscaled PNG")?;
    Ok(BASE64.encode(buf))
}

fn image_call(
    client: &HfClient,
    candidate: &CandidateConfig,
    cfg: &DecodingConfig,
    prompt: &PromptSet,
    image_b64: &str,
) -> Result<(Option<Value>, Usage)> {
    let call = ModelCall {
        model_id: candidate.id.clone(),
        provider: candidate.provider.clone(),
        model_revision: candidate.model_revision.clone(),
        prompt_version: prompt.version.clone(),
        system_prompt: prompt.system_prompt.clone(),
        user_content: vec![
            ContentPart::Text(prompt.user_template.clone()),
            ContentPart::ImageDataUrl {
                mime: "image/png".to_string(),
                base64_data: image_b64.to_string(),
            },
        ],
        temperature: cfg.temperature,
        seed: cfg.seed,
        max_tokens: cfg.max_tokens_vlm,
        json_response: candidate.supports_structured_output,
    };
    let resp = client.chat_completion_cold(&call)?;
    let parsed = parse_json_loose(&resp.raw_text);
    Ok((parsed, resp.usage))
}

/// After this many consecutive call failures for one candidate, stop
/// calling it and mark it unavailable rather than retrying every remaining
/// item at the full timeout (lead directive: a hanging provider must not
/// block the run).
const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

fn run_vlm_candidate(
    client: &HfClient,
    candidate: &CandidateConfig,
    models_cfg: &ModelsConfig,
    gold: &GoldSubset,
    dataset_dir: &Path,
    prompt: &PromptSet,
    runs: u32,
) -> Result<(VlmCandidateReport, HashMap<String, Vec<(String, Option<Value>)>>)> {
    let labeled: Vec<&GoldImage> = gold.images.iter().filter(|i| i.labeled).collect();
    let mut unlabeled_first_run: HashMap<String, Vec<(String, Option<Value>)>> = HashMap::new();
    let mut consecutive_failures = 0u32;
    let mut unavailable = false;

    // --- resolution sweep: 1 run/labeled image per candidate size ---
    let mut sweep = Vec::new();
    'sweep: for &max_dim in &models_cfg.image_preprocessing.candidate_max_dimensions_px {
        let mut acc = FieldAccuracy::default();
        for img in &labeled {
            let path = dataset_dir.join("media/images").join(format!("{}.png", img.image_id));
            let b64 = downscale_and_encode(&path, max_dim)?;
            let outcome = image_call(client, candidate, &models_cfg.decoding, prompt, &b64);
            let parsed = match outcome {
                Ok((parsed, _usage)) => {
                    consecutive_failures = 0;
                    parsed
                }
                Err(e) => {
                    eprintln!("  [{}] sweep call failed for {} @ {max_dim}px: {e}", candidate.id, img.image_id);
                    consecutive_failures += 1;
                    if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
                        eprintln!("  [{}] {CIRCUIT_BREAKER_THRESHOLD} consecutive failures -> marking unavailable, skipping rest", candidate.id);
                        unavailable = true;
                        break 'sweep;
                    }
                    None
                }
            };
            if let Some(expected) = &img.expected_figures {
                acc.add(compare_flat_object(expected, parsed.as_ref()));
            }
        }
        sweep.push((max_dim, acc.rate()));
        eprintln!(
            "  [{}] resolution {max_dim}px -> labeled field accuracy {:.1}%",
            candidate.id,
            acc.rate() * 100.0
        );
    }
    // Smallest resolution within 1 percentage point of the best observed accuracy.
    let best_rate = sweep.iter().map(|(_, r)| *r).fold(0.0, f64::max);
    let chosen_max_dim = sweep
        .iter()
        .filter(|(_, r)| *r >= best_rate - 0.01)
        .map(|(d, _)| *d)
        .min()
        .unwrap_or_else(|| models_cfg.image_preprocessing.candidate_max_dimensions_px.last().copied().unwrap_or(1024));

    // --- full pass at chosen resolution: N runs over all 16 images ---
    let mut stats = RunStats::default();
    let mut selector = SelectorStats::default();
    let mut per_image_outputs: HashMap<String, Vec<Option<Value>>> = HashMap::new();
    if !unavailable {
        consecutive_failures = 0;
        'runs: for run_idx in 0..runs {
            for img in &gold.images {
                let path = dataset_dir.join("media/images").join(format!("{}.png", img.image_id));
                let b64 = downscale_and_encode(&path, chosen_max_dim)?;
                let outcome = image_call(client, candidate, &models_cfg.decoding, prompt, &b64);
                let parsed = match outcome {
                    Ok((parsed, usage)) => {
                        consecutive_failures = 0;
                        stats.record_usage(&usage);
                        if parsed.is_some() {
                            stats.valid_json += 1;
                        }
                        parsed
                    }
                    Err(e) => {
                        eprintln!(
                            "  [{}] run {} call failed for {}: {e}",
                            candidate.id,
                            run_idx + 1,
                            img.image_id
                        );
                        stats.record_failure();
                        consecutive_failures += 1;
                        if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
                            eprintln!("  [{}] {CIRCUIT_BREAKER_THRESHOLD} consecutive failures -> marking unavailable, skipping rest", candidate.id);
                            unavailable = true;
                            break 'runs;
                        }
                        None
                    }
                };
                if let Some(expected) = &img.expected_figures {
                    stats.field_acc.add(compare_flat_object(expected, parsed.as_ref()));
                }
                selector.record(img, parsed.as_ref());
                if !img.labeled && run_idx == 0 {
                    unlabeled_first_run
                        .entry(img.image_id.clone())
                        .or_default()
                        .push((candidate.id.clone(), parsed.clone()));
                }
                per_image_outputs.entry(img.image_id.clone()).or_default().push(parsed);
            }
        }
    }

    // stability: fraction of images whose N parsed outputs are all identical
    let mut stable_images = 0usize;
    let total_images = per_image_outputs.len().max(1);
    for outputs in per_image_outputs.values() {
        if let Some(first) = outputs.first() {
            if outputs.iter().all(|o| o == first) {
                stable_images += 1;
            }
        }
    }
    let stability_rate = if per_image_outputs.is_empty() {
        0.0
    } else {
        stable_images as f64 / total_images as f64
    };

    Ok((
        VlmCandidateReport {
            id: candidate.id.clone(),
            provider: candidate.provider.clone(),
            chosen_max_dim,
            resolution_sweep: sweep,
            stats,
            stability_rate,
            pricing: candidate.pricing_usd_per_m_tokens.clone(),
            unavailable,
            selector,
        },
        unlabeled_first_run,
    ))
}

// ---------------------------------------------------------------------------
// LLM: all gold messages batched into one call per run (PLAN.md §3 batching
// lever — the harness itself must not defeat the lever it is judging).
// ---------------------------------------------------------------------------

fn run_llm_candidate(
    client: &HfClient,
    candidate: &CandidateConfig,
    decoding: &DecodingConfig,
    gold: &GoldSubset,
    prompt: &PromptSet,
    runs: u32,
) -> Result<LlmCandidateReport> {
    let messages_block: String = gold
        .messages
        .iter()
        .map(|m| format!("[{}] {}", m.message_id, m.text))
        .collect::<Vec<_>>()
        .join("\n");
    let user_content = prompt.user_template.replace("{{MESSAGES_BLOCK}}", &messages_block);

    let mut stats = RunStats::default();
    let mut outputs: Vec<Option<Value>> = Vec::new();
    let mut consecutive_failures = 0u32;
    let mut unavailable = false;

    for _run in 0..runs {
        if unavailable {
            break;
        }
        let call = ModelCall {
            model_id: candidate.id.clone(),
            provider: candidate.provider.clone(),
            model_revision: candidate.model_revision.clone(),
            prompt_version: prompt.version.clone(),
            system_prompt: prompt.system_prompt.clone(),
            user_content: vec![ContentPart::Text(user_content.clone())],
            temperature: decoding.temperature,
            seed: decoding.seed,
            // A 47-message batch easily needs several thousand output tokens
            // (and reasoning models burn extra hidden tokens before any
            // visible content — confirmed against openai/gpt-oss-120b during
            // hf.rs's smoke test). This is a bake-off-scale batch; a real
            // per-user batch is far smaller and uses the config default.
            max_tokens: 6000,
            json_response: candidate.supports_structured_output,
        };
        let resp = match client.chat_completion_cold(&call) {
            Ok(resp) => {
                consecutive_failures = 0;
                resp
            }
            Err(e) => {
                eprintln!("  [{}] message-batch call failed: {e}", candidate.id);
                stats.record_failure();
                outputs.push(None);
                consecutive_failures += 1;
                if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
                    eprintln!("  [{}] {CIRCUIT_BREAKER_THRESHOLD} consecutive failures -> marking unavailable, skipping rest", candidate.id);
                    unavailable = true;
                }
                continue;
            }
        };
        stats.record_usage(&resp.usage);
        let parsed = parse_json_loose(&resp.raw_text);
        if parsed.is_some() {
            stats.valid_json += 1;
        }

        if let Some(Value::Array(entries)) = &parsed {
            let by_id: HashMap<String, Vec<Value>> = entries
                .iter()
                .filter_map(|e| {
                    let id = e.get("message_id")?.as_str()?.to_string();
                    let records = e.get("records")?.as_array()?.clone();
                    Some((id, records))
                })
                .collect();
            for gm in &gold.messages {
                let actual = by_id.get(&gm.message_id);
                stats.field_acc.add(compare_message_records(&gm.expected, actual));
            }
        }
        outputs.push(parsed);
    }

    let stability_rate = if outputs.is_empty() {
        0.0
    } else {
        let first = &outputs[0];
        outputs.iter().filter(|o| *o == first).count() as f64 / outputs.len() as f64
    };
    let cost_usd = candidate
        .pricing_usd_per_m_tokens
        .cost_usd(stats.prompt_tokens, stats.completion_tokens);

    Ok(LlmCandidateReport {
        id: candidate.id.clone(),
        provider: candidate.provider.clone(),
        stats,
        stability_rate,
        unavailable,
        cost_usd,
        pricing: candidate.pricing_usd_per_m_tokens.clone(),
    })
}

// ---------------------------------------------------------------------------
// Cross-model agreement on unlabeled images: for each field, does each
// candidate's value match the majority value among candidates that returned
// valid JSON for that image?
// ---------------------------------------------------------------------------

fn cross_model_agreement(unlabeled_first_run: &HashMap<String, Vec<(String, Option<Value>)>>) -> f64 {
    let mut agree = 0u64;
    let mut total = 0u64;
    for outputs in unlabeled_first_run.values() {
        let valid: Vec<&Value> = outputs.iter().filter_map(|(_, v)| v.as_ref()).collect();
        if valid.len() < 2 {
            continue;
        }
        let mut keys: Vec<&String> = Vec::new();
        for v in &valid {
            if let Some(obj) = v.as_object() {
                for k in obj.keys() {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
            }
        }
        for key in keys {
            let mut counts: HashMap<String, u32> = HashMap::new();
            for v in &valid {
                let val = v.get(key).cloned().unwrap_or(Value::Null);
                *counts.entry(val.to_string()).or_insert(0) += 1;
            }
            let majority = counts.values().copied().max().unwrap_or(0);
            for v in &valid {
                total += 1;
                let val = v.get(key).cloned().unwrap_or(Value::Null);
                if counts.get(&val.to_string()).copied().unwrap_or(0) == majority {
                    agree += 1;
                }
            }
        }
    }
    if total == 0 {
        1.0
    } else {
        agree as f64 / total as f64
    }
}

// ---------------------------------------------------------------------------
// Report rendering: splice a "## Step 3 — Bake-off run" ... "## Step 4"
// section into the existing docs/bakeoff.md, preserving everything else.
// ---------------------------------------------------------------------------

/// Non-binding: ml-engineer reports evidence, the user makes the final pick
/// (PLAN.md Phase 2d). Simple weighted heuristic over the metrics that matter
/// most for this challenge (accuracy first, efficiency as the tiebreaker):
/// field accuracy (0.4) + stability (0.25) + valid-JSON (0.2) - normalized
/// cost (0.15, cheaper is better, scaled against the group's max cost/item).
fn recommend<'a, T>(
    items: &'a [T],
    field_acc: impl Fn(&T) -> f64,
    stability: impl Fn(&T) -> f64,
    valid_json: impl Fn(&T) -> f64,
    cost_per_item: impl Fn(&T) -> f64,
) -> Option<&'a T> {
    let max_cost = items.iter().map(&cost_per_item).fold(0.0_f64, f64::max).max(1e-9);
    items.iter().max_by(|a, b| {
        let score = |x: &T| -> f64 {
            0.40 * field_acc(x) + 0.25 * stability(x) + 0.20 * valid_json(x)
                + 0.15 * (1.0 - cost_per_item(x) / max_cost)
        };
        score(a).partial_cmp(&score(b)).unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn render_intro(runs: u32) -> String {
    let mut s = String::new();
    s.push_str("## Step 3 — Bake-off run (results)\n\n");
    s.push_str(&format!(
        "Each candidate run {runs}x at `temperature=0`, fixed `seed`, against the identical gold subset with identical prompts (PLAN.md Phase 2d). Scored against `docs/gold_subset.json` as of `3f1c26a` (message_10 corrected to `salary_first_confirmed` in `3df3082`; image_10/image_11 gold added in `3f1c26a`, 7 labeled + 9 unlabeled images). Nothing here is a pick — the user chooses.\n\n"
    ));
    s.push_str(
        "**Production weight note (from the lead):** extraction's deterministic parser now covers 214/215 messages; the LLM is called for exactly 1 message (`msg_86`) in production, while the VLM is called for all 16 images. Weight the VLM table far more heavily than the LLM table when choosing — the VLM choice is the one that matters at scale.\n\n",
    );
    s
}

fn render_vlm_section(
    vlm: &[VlmCandidateReport],
    agreement: f64,
    runs: u32,
    labeled_count: usize,
    unlabeled_count: usize,
) -> String {
    let mut s = String::new();
    s.push_str("### VLM candidates (image -> typed figure schema)\n\n");
    s.push_str(&format!("| Model | Provider | Chosen res. (px) | Field accuracy vs gold ({labeled_count} labeled) | Valid-JSON rate | Stability ({runs} runs, {} images) | Cross-model agreement ({unlabeled_count} unlabeled) | Avg input tok/item | Avg output tok/item | p50 latency (ms) | Est. cost/item | Est. cost/full run ({} images) |\n", labeled_count + unlabeled_count, FULL_RUN_IMAGES));
    s.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for r in vlm {
        if r.unavailable {
            s.push_str(&format!(
                "| {} | {} | — | UNAVAILABLE | UNAVAILABLE | UNAVAILABLE | — | — | — | — | — | — |\n",
                r.id, r.provider
            ));
            continue;
        }
        let cost_per_item = r.pricing.cost_usd(
            r.stats.avg_prompt_tokens() as u64,
            r.stats.avg_completion_tokens() as u64,
        );
        s.push_str(&format!(
            "| {} | {} | {} | {:.1}% | {:.1}% | {:.1}% | {:.1}% | {:.0} | {:.0} | {:.0} | ${:.5} | ${:.4} |\n",
            r.id,
            r.provider,
            r.chosen_max_dim,
            r.stats.field_acc.rate() * 100.0,
            r.stats.valid_json_rate() * 100.0,
            r.stability_rate * 100.0,
            agreement * 100.0,
            r.stats.avg_prompt_tokens(),
            r.stats.avg_completion_tokens(),
            r.stats.p50_latency_ms(),
            cost_per_item,
            cost_per_item * FULL_RUN_IMAGES as f64,
        ));
    }
    s.push_str(
        "\n**Selected-figure accuracy** (lead directive: the one amount the engine's deterministic selector would actually use matters more than raw all-field accuracy) **and reconciliation** (image_transcription.v1.md's arithmetic checks — subtotal+tax=total, gross-deductions=net, paid+balance=total, line-item sum, currency match):\n\n",
    );
    s.push_str("| Model | Selected-figure accuracy | Reconciliation pass rate | Reconciliation catch rate (wrong reads it would have flagged) |\n");
    s.push_str("|---|---|---|---|\n");
    for r in vlm {
        if r.unavailable {
            s.push_str(&format!("| {} | UNAVAILABLE | UNAVAILABLE | UNAVAILABLE |\n", r.id));
            continue;
        }
        s.push_str(&format!(
            "| {} | {:.1}% ({}/{}) | {:.1}% ({}/{}) | {:.1}% ({}/{}) |\n",
            r.id,
            r.selector.selected_figure_accuracy() * 100.0,
            r.selector.selected_correct,
            r.selector.checked,
            r.selector.reconciliation_pass_rate() * 100.0,
            r.selector.reconciliation_passed,
            r.selector.reconciliation_checked,
            r.selector.reconciliation_catch_rate() * 100.0,
            r.selector.wrong_reads_caught_by_reconciliation,
            r.selector.wrong_reads,
        ));
    }

    let available: Vec<&VlmCandidateReport> = vlm.iter().filter(|r| !r.unavailable).collect();
    if let Some(best) = recommend(
        &available,
        |r: &&VlmCandidateReport| {
            if r.selector.checked > 0 {
                r.selector.selected_figure_accuracy()
            } else {
                r.stats.field_acc.rate()
            }
        },
        |r: &&VlmCandidateReport| r.stability_rate,
        |r: &&VlmCandidateReport| r.stats.valid_json_rate(),
        |r: &&VlmCandidateReport| r.pricing.cost_usd(r.stats.avg_prompt_tokens() as u64, r.stats.avg_completion_tokens() as u64),
    ) {
        s.push_str(&format!(
            "\n**ml-engineer recommendation (VLM, non-binding — the user decides):** `{}` via `{}` at {}px. Highest weighted score, using selected-figure accuracy (not raw all-field accuracy) as the accuracy term per the lead's directive, plus stability, valid-JSON rate, and cost/item; re-check against the actual numbers above before deciding.\n",
            best.id, best.provider, best.chosen_max_dim
        ));
    }
    s.push_str("\nResolution sweep detail (labeled-image field accuracy per candidate max dimension):\n\n");
    for r in vlm {
        if r.unavailable {
            s.push_str(&format!("- `{}`: UNAVAILABLE (marked unavailable after repeated call failures)\n", r.id));
            continue;
        }
        let sweep_str: Vec<String> = r
            .resolution_sweep
            .iter()
            .map(|(d, acc)| format!("{d}px={:.0}%", acc * 100.0))
            .collect();
        s.push_str(&format!("- `{}`: {}\n", r.id, sweep_str.join(", ")));
    }
    s
}

fn render_llm_section(llm: &[LlmCandidateReport], runs: u32) -> String {
    let mut s = String::new();
    s.push_str("\n### LLM candidates (message -> typed records)\n\n");
    s.push_str(&format!("| Model | Provider | Field accuracy vs gold (47 labeled) | Valid-JSON rate | Stability ({runs} runs) | Cross-model agreement | Avg input tok/item | Avg output tok/item | p50 latency (ms) | Est. cost/item | Est. cost/full run |\n"));
    s.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
    const GOLD_MESSAGES_IN_BATCH: f64 = 47.0;
    for r in llm {
        if r.unavailable {
            s.push_str(&format!(
                "| {} | {} | UNAVAILABLE | UNAVAILABLE | UNAVAILABLE | — | — | — | — | — | — |\n",
                r.id, r.provider
            ));
            continue;
        }
        let per_item_tokens_in = r.stats.avg_prompt_tokens() / GOLD_MESSAGES_IN_BATCH;
        let per_item_tokens_out = r.stats.avg_completion_tokens() / GOLD_MESSAGES_IN_BATCH;
        let per_item_cost = r.pricing.cost_usd(per_item_tokens_in as u64, per_item_tokens_out as u64);
        s.push_str(&format!(
            "| {} | {} | {:.1}% | {:.1}% | {:.1}% | N/A (all 47 gold messages labeled) | {:.0} | {:.0} | {:.0} | ${:.5} | ${:.4} |\n",
            r.id,
            r.provider,
            r.stats.field_acc.rate() * 100.0,
            r.stats.valid_json_rate() * 100.0,
            r.stability_rate * 100.0,
            per_item_tokens_in,
            per_item_tokens_out,
            r.stats.p50_latency_ms(),
            per_item_cost,
            per_item_cost * FULL_RUN_MESSAGE_SKELETONS_ESTIMATE as f64,
        ));
    }
    let available: Vec<&LlmCandidateReport> = llm.iter().filter(|r| !r.unavailable).collect();
    if let Some(best) = recommend(
        &available,
        |r: &&LlmCandidateReport| r.stats.field_acc.rate(),
        |r: &&LlmCandidateReport| r.stability_rate,
        |r: &&LlmCandidateReport| r.stats.valid_json_rate(),
        |r: &&LlmCandidateReport| r.cost_usd,
    ) {
        s.push_str(&format!(
            "\n**ml-engineer recommendation (LLM, non-binding — the user decides):** `{}` via `{}`. Highest weighted score across field accuracy, stability, valid-JSON rate, and cost; re-check against the actual field-accuracy/cost numbers above before deciding. Given production LLM volume is now just 1 message (`msg_86`), this pick matters far less than the VLM pick above.\n",
            best.id, best.provider
        ));
    }
    s.push_str(&format!(
        "\n\"Est. cost/full run\" for messages uses {FULL_RUN_MESSAGE_SKELETONS_ESTIMATE} as a lower-bound proxy from the gold subset's distinct `record_type` shapes — now superseded by the lead's harder number: extraction's deterministic parser covers 214/215 messages, so production LLM volume is 1 message (`msg_86`), not {FULL_RUN_MESSAGE_SKELETONS_ESTIMATE}.\n\n"
    ));
    s.push_str("(Bake-off messages are batched one call per run for the whole 47-message gold subset, matching the batching lever being judged; a real per-user batch in production is far smaller — per-item token/cost figures above divide the batch call by its message count.)\n\n");
    s
}

fn splice_into_bakeoff_md(out_path: &Path, new_section: &str) -> Result<()> {
    let existing = fs::read_to_string(out_path).unwrap_or_default();
    let start_marker = "## Step 3 — Bake-off run";
    let end_marker = "## Step 4";
    let spliced = match (existing.find(start_marker), existing.find(end_marker)) {
        (Some(start), Some(end)) if end > start => {
            format!("{}{}\n{}", &existing[..start], new_section, &existing[end..])
        }
        _ => format!("{existing}\n{new_section}"),
    };
    fs::write(out_path, spliced)?;
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args();

    let config_text = fs::read_to_string(&args.config)
        .with_context(|| format!("reading {}", args.config.display()))?;
    let cfg: ModelsConfig = toml::from_str(&config_text)
        .with_context(|| format!("parsing {}", args.config.display()))?;

    let gold_text = fs::read_to_string(&args.gold_subset)
        .with_context(|| format!("reading {}", args.gold_subset.display()))?;
    let gold: GoldSubset = serde_json::from_str(&gold_text)
        .with_context(|| format!("parsing {}", args.gold_subset.display()))?;

    let image_prompt = load_prompt(
        &args.prompts_dir.join("image_transcription.v1.md"),
        "User prompt template",
    )?;
    let message_prompt = load_prompt(
        &args.prompts_dir.join("message_extraction.v1.md"),
        "User prompt template",
    )?;

    // Bake-off-specific policy, deliberately tighter than production's
    // config/models.toml [retry]: a hard 60s per-request timeout and few
    // retries, so one hanging provider costs seconds, not the whole run
    // (lead directive). Production runs use the fuller retry policy from
    // models.toml via their own HfClient.
    let client = HfClient::with_cache_dir(&args.cache_dir)?
        .with_request_timeout(60)?
        .with_retry_policy(2, 1000, 2.0, 4000);

    eprintln!(
        "bake-off: {} VLM candidate(s), {} LLM candidate(s), {} runs each, {} labeled + {} unlabeled images, {} messages (parallel per modality, 60s/req timeout, circuit breaker at {CIRCUIT_BREAKER_THRESHOLD} consecutive failures)",
        cfg.candidates.vlm.len(),
        cfg.candidates.llm.len(),
        args.runs,
        gold.images.iter().filter(|i| i.labeled).count(),
        gold.images.iter().filter(|i| !i.labeled).count(),
        gold.messages.len(),
    );

    // --- VLM candidates run in parallel (they carry nearly all production
    // calls per the lead; report this table first, before LLM starts). ---
    fn unavailable_vlm_report(candidate: &CandidateConfig) -> VlmCandidateReport {
        VlmCandidateReport {
            id: candidate.id.clone(),
            provider: candidate.provider.clone(),
            chosen_max_dim: 0,
            resolution_sweep: Vec::new(),
            stats: RunStats::default(),
            stability_rate: 0.0,
            pricing: candidate.pricing_usd_per_m_tokens.clone(),
            unavailable: true,
            selector: SelectorStats::default(),
        }
    }

    let vlm_results: Vec<(VlmCandidateReport, HashMap<String, Vec<(String, Option<Value>)>>)> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = cfg
                .candidates
                .vlm
                .iter()
                .map(|candidate| {
                    scope.spawn(|| {
                        eprintln!("=== VLM candidate: {} ({}) ===", candidate.id, candidate.provider);
                        run_vlm_candidate(&client, candidate, &cfg, &gold, &args.dataset_dir, &image_prompt, args.runs)
                    })
                })
                .collect();
            handles
                .into_iter()
                .zip(&cfg.candidates.vlm)
                .map(|(h, candidate)| match h.join() {
                    Ok(Ok((report, unlabeled))) => (report, unlabeled),
                    Ok(Err(e)) => {
                        eprintln!("=== VLM candidate {} failed outright: {e} -> marking unavailable ===", candidate.id);
                        (unavailable_vlm_report(candidate), HashMap::new())
                    }
                    Err(_) => {
                        eprintln!("=== VLM candidate {} thread panicked -> marking unavailable ===", candidate.id);
                        (unavailable_vlm_report(candidate), HashMap::new())
                    }
                })
                .collect()
        });

    let mut vlm_reports: Vec<VlmCandidateReport> = Vec::new();
    let mut unlabeled_first_run: HashMap<String, Vec<(String, Option<Value>)>> = HashMap::new();
    for (report, unlabeled) in vlm_results {
        for (image_id, entries) in unlabeled {
            unlabeled_first_run.entry(image_id).or_default().extend(entries);
        }
        vlm_reports.push(report);
    }
    let agreement = cross_model_agreement(&unlabeled_first_run);

    let section_intro = render_intro(args.runs);
    let labeled_count = gold.images.iter().filter(|i| i.labeled).count();
    let unlabeled_count = gold.images.iter().filter(|i| !i.labeled).count();
    let vlm_section = render_vlm_section(&vlm_reports, agreement, args.runs, labeled_count, unlabeled_count);
    splice_into_bakeoff_md(&args.out, &format!("{section_intro}{vlm_section}"))?;
    eprintln!("bake-off: VLM table written into {} (LLM section running next)", args.out.display());

    // --- LLM candidates: low production stakes now (1 msg in prod), but
    // still run in parallel for speed. ---
    let llm_reports: Vec<LlmCandidateReport> = std::thread::scope(|scope| {
        let handles: Vec<_> = cfg
            .candidates
            .llm
            .iter()
            .map(|candidate| {
                scope.spawn(|| {
                    eprintln!("=== LLM candidate: {} ({}) ===", candidate.id, candidate.provider);
                    run_llm_candidate(&client, candidate, &cfg.decoding, &gold, &message_prompt, args.runs)
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(&cfg.candidates.llm)
            .map(|(h, candidate)| match h.join() {
                Ok(Ok(report)) => report,
                Ok(Err(e)) => {
                    eprintln!("=== LLM candidate {} failed outright: {e} -> marking unavailable ===", candidate.id);
                    LlmCandidateReport {
                        id: candidate.id.clone(),
                        provider: candidate.provider.clone(),
                        stats: RunStats::default(),
                        stability_rate: 0.0,
                        unavailable: true,
                        cost_usd: 0.0,
                        pricing: candidate.pricing_usd_per_m_tokens.clone(),
                    }
                }
                Err(_) => {
                    eprintln!("=== LLM candidate {} thread panicked -> marking unavailable ===", candidate.id);
                    LlmCandidateReport {
                        id: candidate.id.clone(),
                        provider: candidate.provider.clone(),
                        stats: RunStats::default(),
                        stability_rate: 0.0,
                        unavailable: true,
                        cost_usd: 0.0,
                        pricing: candidate.pricing_usd_per_m_tokens.clone(),
                    }
                }
            })
            .collect()
    });

    let llm_section = render_llm_section(&llm_reports, args.runs);
    splice_into_bakeoff_md(&args.out, &format!("{section_intro}{vlm_section}{llm_section}"))?;
    eprintln!("bake-off: full report written into {}", args.out.display());

    Ok(())
}
