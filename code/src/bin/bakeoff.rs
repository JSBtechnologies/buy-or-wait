//! Model bake-off harness (owner: ml-engineer). PLAN.md §3 / Phase 2d.
//!
//! Runs every VLM candidate against `docs/gold_subset.json` images (5 labeled
//! + 11 unlabeled), sweeping image resolution to find the smallest size that
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
// Aggregated stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct RunStats {
    calls: u32,
    valid_json: u32,
    prompt_tokens: u64,
    completion_tokens: u64,
    latency_ms_sum: u64,
    field_acc: FieldAccuracy,
}

impl RunStats {
    fn record_usage(&mut self, usage: &Usage) {
        self.calls += 1;
        self.prompt_tokens += usage.prompt_tokens;
        self.completion_tokens += usage.completion_tokens;
        self.latency_ms_sum += usage.latency_ms;
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
    fn avg_latency_ms(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.latency_ms_sum as f64 / self.calls as f64
        }
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
    cost_usd: f64,
}

struct LlmCandidateReport {
    id: String,
    provider: String,
    stats: RunStats,
    stability_rate: f64,
    cost_usd: f64,
}

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

fn run_vlm_candidate(
    client: &HfClient,
    candidate: &CandidateConfig,
    models_cfg: &ModelsConfig,
    gold: &GoldSubset,
    dataset_dir: &Path,
    prompt: &PromptSet,
    runs: u32,
    unlabeled_first_run: &mut HashMap<String, Vec<(String, Option<Value>)>>,
) -> Result<VlmCandidateReport> {
    let labeled: Vec<&GoldImage> = gold.images.iter().filter(|i| i.labeled).collect();

    // --- resolution sweep: 1 run/labeled image per candidate size ---
    let mut sweep = Vec::new();
    for &max_dim in &models_cfg.image_preprocessing.candidate_max_dimensions_px {
        let mut acc = FieldAccuracy::default();
        for img in &labeled {
            let path = dataset_dir.join("media/images").join(format!("{}.png", img.image_id));
            let b64 = downscale_and_encode(&path, max_dim)?;
            let (parsed, _usage) = image_call(client, candidate, &models_cfg.decoding, prompt, &b64)?;
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
    let mut per_image_outputs: HashMap<String, Vec<Option<Value>>> = HashMap::new();
    for run_idx in 0..runs {
        for img in &gold.images {
            let path = dataset_dir.join("media/images").join(format!("{}.png", img.image_id));
            let b64 = downscale_and_encode(&path, chosen_max_dim)?;
            let (parsed, usage) = image_call(client, candidate, &models_cfg.decoding, prompt, &b64)?;
            stats.record_usage(&usage);
            if parsed.is_some() {
                stats.valid_json += 1;
            }
            if let Some(expected) = &img.expected_figures {
                stats.field_acc.add(compare_flat_object(expected, parsed.as_ref()));
            }
            if !img.labeled {
                if run_idx == 0 {
                    unlabeled_first_run
                        .entry(img.image_id.clone())
                        .or_default()
                        .push((candidate.id.clone(), parsed.clone()));
                }
            }
            per_image_outputs.entry(img.image_id.clone()).or_default().push(parsed);
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
    let stability_rate = stable_images as f64 / total_images as f64;

    let cost_usd = candidate
        .pricing_usd_per_m_tokens
        .cost_usd(stats.prompt_tokens, stats.completion_tokens);

    Ok(VlmCandidateReport {
        id: candidate.id.clone(),
        provider: candidate.provider.clone(),
        chosen_max_dim,
        resolution_sweep: sweep,
        stats,
        stability_rate,
        cost_usd,
    })
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

    for _run in 0..runs {
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
        let resp = client.chat_completion_cold(&call)?;
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
        cost_usd,
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

fn render_report(vlm: &[VlmCandidateReport], llm: &[LlmCandidateReport], agreement: f64, runs: u32) -> String {
    let mut s = String::new();
    s.push_str("## Step 3 — Bake-off run (results)\n\n");
    s.push_str(&format!(
        "Each candidate run {runs}x at `temperature=0`, fixed `seed`, against the identical gold subset with identical prompts (PLAN.md Phase 2d). Nothing here is a pick — the user chooses.\n\n"
    ));

    s.push_str("### VLM candidates (image -> typed figure schema)\n\n");
    s.push_str("| Model | Provider | Chosen resolution (px) | Valid-JSON rate | Field accuracy (5 labeled) | Stability (16 images) | Avg prompt tok | Avg completion tok | Avg latency (ms) | Total cost (subset) |\n");
    s.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    for r in vlm {
        s.push_str(&format!(
            "| {} | {} | {} | {:.1}% | {:.1}% | {:.1}% | {:.0} | {:.0} | {:.0} | ${:.4} |\n",
            r.id,
            r.provider,
            r.chosen_max_dim,
            r.stats.valid_json_rate() * 100.0,
            r.stats.field_acc.rate() * 100.0,
            r.stability_rate * 100.0,
            r.stats.avg_prompt_tokens(),
            r.stats.avg_completion_tokens(),
            r.stats.avg_latency_ms(),
            r.cost_usd,
        ));
    }
    s.push_str(&format!(
        "\nCross-model agreement on the 11 unlabeled images (no ground truth; majority-vote field agreement across candidates): **{:.1}%**.\n\n",
        agreement * 100.0
    ));
    s.push_str("Resolution sweep detail (labeled-image field accuracy per candidate max dimension):\n\n");
    for r in vlm {
        let sweep_str: Vec<String> = r
            .resolution_sweep
            .iter()
            .map(|(d, acc)| format!("{d}px={:.0}%", acc * 100.0))
            .collect();
        s.push_str(&format!("- `{}`: {}\n", r.id, sweep_str.join(", ")));
    }

    s.push_str("\n### LLM candidates (message -> typed records)\n\n");
    s.push_str("| Model | Provider | Valid-JSON rate | Field accuracy (47 labeled) | Stability | Avg prompt tok | Avg completion tok | Avg latency (ms) | Total cost (subset) |\n");
    s.push_str("|---|---|---|---|---|---|---|---|---|\n");
    for r in llm {
        s.push_str(&format!(
            "| {} | {} | {:.1}% | {:.1}% | {:.1}% | {:.0} | {:.0} | {:.0} | ${:.4} |\n",
            r.id,
            r.provider,
            r.stats.valid_json_rate() * 100.0,
            r.stats.field_acc.rate() * 100.0,
            r.stability_rate * 100.0,
            r.stats.avg_prompt_tokens(),
            r.stats.avg_completion_tokens(),
            r.stats.avg_latency_ms(),
            r.cost_usd,
        ));
    }
    s.push_str("\n(LLM messages are batched one call per run for the whole 47-message gold subset, per PLAN.md §3's batching lever; a real per-user batch in production is far smaller.)\n\n");
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

    let client = HfClient::with_cache_dir(&args.cache_dir)?.with_retry_policy(
        cfg.retry.max_attempts,
        cfg.retry.backoff_base_ms,
        cfg.retry.backoff_multiplier,
        cfg.retry.backoff_max_ms,
    );

    eprintln!(
        "bake-off: {} VLM candidate(s), {} LLM candidate(s), {} runs each, {} labeled + {} unlabeled images, {} messages",
        cfg.candidates.vlm.len(),
        cfg.candidates.llm.len(),
        args.runs,
        gold.images.iter().filter(|i| i.labeled).count(),
        gold.images.iter().filter(|i| !i.labeled).count(),
        gold.messages.len(),
    );

    let mut unlabeled_first_run: HashMap<String, Vec<(String, Option<Value>)>> = HashMap::new();
    let mut vlm_reports = Vec::new();
    for candidate in &cfg.candidates.vlm {
        eprintln!("=== VLM candidate: {} ({}) ===", candidate.id, candidate.provider);
        let report = run_vlm_candidate(
            &client,
            candidate,
            &cfg,
            &gold,
            &args.dataset_dir,
            &image_prompt,
            args.runs,
            &mut unlabeled_first_run,
        )?;
        vlm_reports.push(report);
    }
    let agreement = cross_model_agreement(&unlabeled_first_run);

    let mut llm_reports = Vec::new();
    for candidate in &cfg.candidates.llm {
        eprintln!("=== LLM candidate: {} ({}) ===", candidate.id, candidate.provider);
        let report = run_llm_candidate(&client, candidate, &cfg.decoding, &gold, &message_prompt, args.runs)?;
        llm_reports.push(report);
    }

    let section = render_report(&vlm_reports, &llm_reports, agreement, args.runs);
    splice_into_bakeoff_md(&args.out, &section)?;
    eprintln!("bake-off: results written into {}", args.out.display());

    Ok(())
}
