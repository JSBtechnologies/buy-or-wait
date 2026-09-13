//! HF Inference Providers client (owner: ml-engineer).
//!
//! Blocking, OpenAI-compatible client for the Hugging Face router
//! (`https://router.huggingface.co/v1/chat/completions`). All Buy or Wait?
//! model calls go through here: images (§2.3), messages (§2.4), and text
//! request intake (§2.6). Nothing here decides anything (§2.5) — it returns
//! raw model text plus usage; the caller (extraction) deserializes into a
//! typed struct with fixed fields.
//!
//! Every call is:
//! - content-addressed and disk-cached, so a repeated document/question costs
//!   zero tokens on rerun (PLAN.md §2.11, §3 Caching lever);
//! - retried with backoff on 429/5xx;
//! - recorded as a `Usage` (model, provider, prompt/completion tokens,
//!   latency) for the token/cost report (PLAN.md §3, §6.5).
//!
//! `HF_TOKEN` is read from the environment only, and is never logged, printed,
//! or included in any cached file or error message.

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ROUTER_CHAT_COMPLETIONS_URL: &str = "https://router.huggingface.co/v1/chat/completions";
const ROUTER_MODELS_URL: &str = "https://router.huggingface.co/v1/models";
const HF_TOKEN_ENV: &str = "HF_TOKEN";

/// One part of the user message content: plain text, or a downscaled image
/// already encoded as base64 (PLAN.md §3 token-efficiency / image
/// downscaling lever — encoding happens before this point, in extraction).
#[derive(Debug, Clone)]
pub enum ContentPart {
    Text(String),
    ImageDataUrl { mime: String, base64_data: String },
}

/// Everything needed to identify, cache-key, and pin one model call.
///
/// `system_prompt` must be byte-identical across every call to the same
/// `model_id` + `prompt_version` so HF providers can apply prefix/prompt
/// caching (PLAN.md §2.11): only `user_content` varies per call.
#[derive(Debug, Clone)]
pub struct ModelCall {
    pub model_id: String,
    pub provider: String,
    pub model_revision: String,
    pub prompt_version: String,
    pub system_prompt: String,
    pub user_content: Vec<ContentPart>,
    pub temperature: f64,
    pub seed: i64,
    pub max_tokens: u32,
    pub json_response: bool,
    /// Strict JSON Schema for structured output, when the backend needs the
    /// actual schema rather than just "respond in JSON" (e.g. Anthropic's
    /// `output_config.format.schema` — `crate::anthropic`). `None` for
    /// backends that only need `json_response` as a boolean flag (HF's
    /// `response_format: {"type": "json_object"}`).
    pub json_schema: Option<serde_json::Value>,
}

/// Per-call usage record for the cost/token report (PLAN.md §6.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub model_id: String,
    pub provider: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u64,
    pub cache_hit: bool,
}

/// A model's raw text output plus its usage. The caller owns parsing this
/// into a typed, validated struct (§2.5: no free-text channel into the
/// engine).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub raw_text: String,
    pub usage: Usage,
}

enum HfCallError {
    /// HTTP 429 or 5xx: worth retrying with backoff.
    Retryable(u16),
    /// Anything else (bad request, parse failure, network error): not worth retrying.
    Fatal(String),
}

impl fmt::Display for HfCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HfCallError::Retryable(status) => write!(f, "retryable HTTP {status}"),
            HfCallError::Fatal(msg) => write!(f, "{msg}"),
        }
    }
}

/// Content hash of a call's variable inputs only. Model identity and prompt
/// version are folded in separately by `cache_key_for` so identical content
/// under a different model or prompt version gets its own cache entry.
fn content_hash(call: &ModelCall) -> String {
    let mut hasher = Sha256::new();
    for part in &call.user_content {
        match part {
            ContentPart::Text(t) => {
                hasher.update(b"text:");
                hasher.update(t.as_bytes());
            }
            ContentPart::ImageDataUrl { mime, base64_data } => {
                hasher.update(b"image:");
                hasher.update(mime.as_bytes());
                hasher.update(base64_data.as_bytes());
            }
        }
    }
    format!("{:x}", hasher.finalize())
}

/// `sha256(content_hash + model_id + model_revision + prompt_version)` (PLAN.md
/// §2.11). Shared by every backend (`HfClient`, `crate::anthropic::AnthropicClient`)
/// so a call routed to a different provider for the same logical model/prompt
/// still gets its own cache entry, and the formula never drifts between them.
pub(crate) fn cache_key_for(call: &ModelCall) -> String {
    let content_hash = content_hash(call);
    let mut hasher = Sha256::new();
    hasher.update(content_hash.as_bytes());
    hasher.update(call.model_id.as_bytes());
    hasher.update(call.model_revision.as_bytes());
    hasher.update(call.prompt_version.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub struct HfClient {
    http: reqwest::blocking::Client,
    token: String,
    cache_dir: PathBuf,
    max_attempts: u32,
    backoff_base_ms: u64,
    backoff_multiplier: f64,
    backoff_max_ms: u64,
    /// Every successful call's `Usage` (cache hit or live), in call order.
    /// Read back at the end of a run via `usage_records()` to build the
    /// token/cost report (PLAN.md §6.5). A bake-off run and a real pipeline
    /// run each get their own `HfClient`, so their usage logs never mix.
    usage_log: Mutex<Vec<Usage>>,
}

impl HfClient {
    /// Reads `HF_TOKEN` from the environment. Caches under `store/model_cache`
    /// relative to the current working directory (the crate root, `code/`,
    /// when run via `cargo run`), matching `code/store/` in PLAN.md §2.9 —
    /// generated, gitignored, rebuilt by the pipeline.
    pub fn new() -> Result<Self> {
        Self::with_cache_dir("store/model_cache")
    }

    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Result<Self> {
        let token = env::var(HF_TOKEN_ENV)
            .with_context(|| format!("{HF_TOKEN_ENV} env var not set"))?;
        let http = reqwest::blocking::Client::builder()
            // Without a request timeout, a provider that accepts the
            // connection but never responds hangs the call forever — no
            // status code ever arrives, so the 429/5xx retry path never
            // triggers. A bounded timeout turns that into a retryable error
            // instead (`is_timeout()`, handled in `call_once`).
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            // Observed multi-minute hangs with near-zero CPU (i.e. blocked
            // on I/O, past the configured timeout) during bake-off runs —
            // consistent with a pooled keep-alive connection that went
            // stale server-side without the client detecting it. Disabling
            // idle-connection reuse forces a fresh connection per request,
            // which the connect/request timeouts above do reliably bound.
            .pool_max_idle_per_host(0)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            token,
            cache_dir: cache_dir.into(),
            max_attempts: 5,
            backoff_base_ms: 500,
            backoff_multiplier: 2.0,
            backoff_max_ms: 8000,
            usage_log: Mutex::new(Vec::new()),
        })
    }

    /// Every `Usage` recorded so far this run, in call order (cache hits
    /// included, tagged via `Usage::cache_hit`). Feed this into
    /// `render_usage_report` at the end of a full-dataset run to produce
    /// `code/evaluation/usage_report.md` (PLAN.md §6.5).
    pub fn usage_records(&self) -> Vec<Usage> {
        self.usage_log.lock().expect("usage_log mutex poisoned").clone()
    }

    /// A cheap, zero-completion-token pre-flight check (image_accuracy_plan.md §"Live N=5":
    /// "check HF credits first") -- GETs the router's model listing with the same bearer token
    /// a completion call would use. Confirms the token is valid and the router is reachable
    /// BEFORE a run that may fire up to `16 images * 4 reads * N runs` paid completion calls;
    /// a 401/403 here means an invalid/expired token, not exhausted credits per se (the router
    /// does not expose a separate balance endpoint), but either way this is cheaper and faster
    /// to fail on than discovering it mid-sweep.
    pub fn check_router_reachable(&self) -> Result<()> {
        let resp = self
            .http
            .get(ROUTER_MODELS_URL)
            .bearer_auth(&self.token)
            .send()
            .context("HF router unreachable (GET /v1/models)")?;
        if !resp.status().is_success() {
            bail!("HF router GET /v1/models returned {} -- check HF_TOKEN and account status before running a live sweep", resp.status());
        }
        Ok(())
    }

    /// Rebuild the HTTP client with a hard per-request timeout (connect
    /// timeout is `secs / 4`, minimum 5s). Use a tight bound (e.g. the
    /// bake-off's 60s) where a hanging provider must fail fast rather than
    /// block a whole run.
    pub fn with_request_timeout(mut self, secs: u64) -> Result<Self> {
        self.http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs((secs / 4).max(5)))
            .timeout(Duration::from_secs(secs))
            .pool_max_idle_per_host(0)
            .build()
            .context("failed to rebuild HTTP client with custom timeout")?;
        Ok(self)
    }

    /// Override retry/backoff behavior (e.g. for tests). Defaults match
    /// `code/config/models.toml` `[retry]`.
    pub fn with_retry_policy(
        mut self,
        max_attempts: u32,
        backoff_base_ms: u64,
        backoff_multiplier: f64,
        backoff_max_ms: u64,
    ) -> Self {
        self.max_attempts = max_attempts;
        self.backoff_base_ms = backoff_base_ms;
        self.backoff_multiplier = backoff_multiplier;
        self.backoff_max_ms = backoff_max_ms;
        self
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        self.cache_dir.join(format!("{key}.json"))
    }

    fn cache_path_run(&self, key: &str, run_idx: u32) -> PathBuf {
        self.cache_dir.join(format!("{key}__run{run_idx}.json"))
    }

    /// Run one chat completion. Cache-first; on a miss, calls the router
    /// with retry/backoff and writes the result back to the cache.
    pub fn chat_completion(&self, call: &ModelCall) -> Result<ModelResponse> {
        let key = cache_key_for(call);
        let path = self.cache_path(&key);

        if let Ok(bytes) = fs::read(&path) {
            if let Ok(mut cached) = serde_json::from_slice::<ModelResponse>(&bytes) {
                cached.usage.cache_hit = true;
                self.record_usage(&cached.usage);
                return Ok(cached);
            }
        }

        let response = self.call_with_retry(call)?;
        self.write_cache(&path, &response);
        self.record_usage(&response.usage);
        Ok(response)
    }

    /// Same as `chat_completion` but never reads the cache (still writes to
    /// it). Used for `--cold` runs (PLAN.md Phase 3) so the run reflects real
    /// calls while still leaving a warm cache behind for the determinism
    /// check.
    pub fn chat_completion_cold(&self, call: &ModelCall) -> Result<ModelResponse> {
        let response = self.call_with_retry(call)?;
        let key = cache_key_for(call);
        self.write_cache(&self.cache_path(&key), &response);
        self.record_usage(&response.usage);
        Ok(response)
    }

    /// Same as `chat_completion_cold`, but also persists this run's raw
    /// response under its own `{key}__run{run_idx}.json` file, alongside the
    /// canonical `{key}.json` (still overwritten each run, unchanged, so
    /// `--rescore-from-cache` keeps working off the latest response).
    /// Without this, a stability sweep (N runs of the identical call, same
    /// cache key by construction) only ever leaves the *last* run's raw text
    /// on disk -- runs 1..N-1 are silently lost once run N writes over them.
    /// A sign-off audit needs every run's raw response persisted for review
    /// without re-calling the API (analyst request, bus topic `blocker`,
    /// RULES.md#S5).
    pub fn chat_completion_cold_numbered(
        &self,
        call: &ModelCall,
        run_idx: u32,
    ) -> Result<ModelResponse> {
        let response = self.call_with_retry(call)?;
        let key = cache_key_for(call);
        self.write_cache(&self.cache_path(&key), &response);
        self.write_cache(&self.cache_path_run(&key, run_idx), &response);
        self.record_usage(&response.usage);
        Ok(response)
    }

    fn record_usage(&self, usage: &Usage) {
        self.usage_log
            .lock()
            .expect("usage_log mutex poisoned")
            .push(usage.clone());
    }

    fn write_cache(&self, path: &PathBuf, response: &ModelResponse) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(serialized) = serde_json::to_vec_pretty(response) {
            let _ = fs::write(path, serialized);
        }
    }

    fn call_with_retry(&self, call: &ModelCall) -> Result<ModelResponse> {
        let mut attempt = 0u32;
        let mut backoff_ms = self.backoff_base_ms;
        loop {
            attempt += 1;
            match self.call_once(call) {
                Ok(resp) => return Ok(resp),
                Err(HfCallError::Retryable(status)) if attempt < self.max_attempts => {
                    let reason = if status == 0 {
                        "network/transport error".to_string()
                    } else {
                        format!("retryable HTTP {status}")
                    };
                    eprintln!(
                        "hf: {reason} calling {} via {} (attempt {attempt}/{}); backing off {backoff_ms}ms",
                        call.model_id, call.provider, self.max_attempts
                    );
                    thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = (((backoff_ms as f64) * self.backoff_multiplier) as u64)
                        .min(self.backoff_max_ms);
                }
                Err(err) => bail!(
                    "hf: call to {} via {} failed after {attempt} attempt(s): {err}",
                    call.model_id,
                    call.provider
                ),
            }
        }
    }

    fn call_once(&self, call: &ModelCall) -> Result<ModelResponse, HfCallError> {
        // Router provider selection convention: "<repo_id>:<provider>".
        let model = format!("{}:{}", call.model_id, call.provider);

        let content_parts: Vec<serde_json::Value> = call
            .user_content
            .iter()
            .map(|part| match part {
                ContentPart::Text(t) => serde_json::json!({"type": "text", "text": t}),
                ContentPart::ImageDataUrl { mime, base64_data } => serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{mime};base64,{base64_data}") }
                }),
            })
            .collect();

        let mut body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "system", "content": call.system_prompt},
                {"role": "user", "content": content_parts},
            ],
            "temperature": call.temperature,
            "seed": call.seed,
            "max_tokens": call.max_tokens,
        });
        if call.json_response {
            body["response_format"] = serde_json::json!({"type": "json_object"});
        }

        let started = Instant::now();
        let response = self
            .http
            .post(ROUTER_CHAT_COMPLETIONS_URL)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .map_err(|e| {
                // Transport-level failures (DNS blips, connection resets, timeouts)
                // are usually transient, same as a 5xx: worth retrying with backoff.
                // 0 is not a real HTTP status; it just carries the retry decision.
                if e.is_timeout() || e.is_connect() || e.is_request() {
                    HfCallError::Retryable(0)
                } else {
                    HfCallError::Fatal(format!("request failed: {e}"))
                }
            })?;

        let status = response.status();
        if status.as_u16() == 429 || status.is_server_error() {
            return Err(HfCallError::Retryable(status.as_u16()));
        }
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            return Err(HfCallError::Fatal(format!("HTTP {status}: {text}")));
        }

        let parsed: ChatCompletionResponse = response
            .json()
            .map_err(|e| HfCallError::Fatal(format!("failed to parse response JSON: {e}")))?;
        let latency_ms = started.elapsed().as_millis() as u64;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| HfCallError::Fatal("response had no choices".to_string()))?;

        Ok(ModelResponse {
            raw_text: content,
            usage: Usage {
                model_id: call.model_id.clone(),
                provider: call.provider.clone(),
                prompt_tokens: parsed.usage.prompt_tokens,
                completion_tokens: parsed.usage.completion_tokens,
                latency_ms,
                cache_hit: false,
            },
        })
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: ChatUsageRaw,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: String,
}

#[derive(Debug, Default, Deserialize)]
struct ChatUsageRaw {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

// ---------------------------------------------------------------------------
// Usage report (`code/evaluation/usage_report.md`, PLAN.md §6.5)
// ---------------------------------------------------------------------------

/// Per-million-token pricing for one model, mirroring
/// `code/config/models.toml`'s `pricing_usd_per_m_tokens`. Kept as a plain
/// struct here (not parsed from TOML) so `hf.rs` has no dependency on a TOML
/// parser; the caller reads `models.toml` and builds this map, keyed by
/// `model_id`.
#[derive(Debug, Clone, Copy)]
pub struct Pricing {
    pub input_per_m: f64,
    pub output_per_m: f64,
}

impl Pricing {
    pub fn cost_usd(&self, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        (prompt_tokens as f64 / 1_000_000.0) * self.input_per_m
            + (completion_tokens as f64 / 1_000_000.0) * self.output_per_m
    }
}

#[derive(Debug, Default, Clone)]
struct ModelTotals {
    provider: String,
    calls: u64,
    cache_hits: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cost_usd: f64,
}

/// Renders `code/evaluation/usage_report.md` from one run's `Usage` records.
/// `pricing` is keyed by `model_id` (built by the caller from
/// `code/config/models.toml`); a model with no pricing entry is still
/// reported (calls/tokens) with its cost cell shown as "N/A" rather than
/// silently omitted or treated as free. `total_requests` is the number of
/// rows in `dataset/requests.csv` for this run — used only for the
/// per-request averages, never for the per-call averages.
///
/// Renders every required section (Overview, per-model breakdown, Overall
/// row) with zeros when `records` is empty — a baseline run with 0 model
/// calls must still pass contract signoff (PLAN.md §6.5).
pub fn render_usage_report(
    records: &[Usage],
    pricing: &std::collections::HashMap<String, Pricing>,
    total_requests: usize,
) -> String {
    let mut by_model: std::collections::BTreeMap<String, ModelTotals> = std::collections::BTreeMap::new();
    for r in records {
        let entry = by_model.entry(r.model_id.clone()).or_insert_with(|| ModelTotals {
            provider: r.provider.clone(),
            ..Default::default()
        });
        entry.calls += 1;
        if r.cache_hit {
            entry.cache_hits += 1;
        }
        entry.prompt_tokens += r.prompt_tokens;
        entry.completion_tokens += r.completion_tokens;
        if let Some(p) = pricing.get(&r.model_id) {
            entry.cost_usd += p.cost_usd(r.prompt_tokens, r.completion_tokens);
        }
    }

    let total_calls: u64 = by_model.values().map(|m| m.calls).sum();
    let total_cache_hits: u64 = by_model.values().map(|m| m.cache_hits).sum();
    let total_prompt: u64 = by_model.values().map(|m| m.prompt_tokens).sum();
    let total_completion: u64 = by_model.values().map(|m| m.completion_tokens).sum();
    let total_tokens = total_prompt + total_completion;
    // `Sum for f64` folds from `-0.0`, so an empty/all-zero sum is `-0.0`,
    // which would render as the confusing "$-0.000000" on a baseline (0-call)
    // run; `+ 0.0` normalizes it back to `+0.0` (IEEE-754: -0.0 + 0.0 = +0.0).
    let total_cost: f64 = by_model.values().map(|m| m.cost_usd).sum::<f64>() + 0.0;
    let any_unpriced = records.iter().any(|r| !pricing.contains_key(&r.model_id));

    let mut s = String::new();
    s.push_str("# Usage report\n\n");
    s.push_str(
        "Generated from the final full-dataset run that produced `output.csv`. The run \
         starts from an empty cache (`--cold`, PLAN.md \u{a7}2.11/\u{a7}3) so every call counted \
         here is a real model invocation, not a cache hit.\n\n",
    );

    s.push_str("## Overview\n\n");
    s.push_str(&format!("- Requests in this run: {total_requests}\n"));
    s.push_str(&format!("- Model calls: {total_calls}\n"));
    s.push_str(&format!("- Input tokens: {total_prompt}\n"));
    s.push_str(&format!("- Output tokens: {total_completion}\n"));
    s.push_str(&format!("- Total tokens: {total_tokens}\n"));
    if total_calls > 0 {
        s.push_str(&format!(
            "- Cache hit rate: {:.1}% ({total_cache_hits}/{total_calls} calls served from the \u{a7}2.11 disk cache, 0 tokens/cost)\n",
            100.0 * total_cache_hits as f64 / total_calls as f64
        ));
    } else {
        s.push_str("- Cache hit rate: N/A (0 calls)\n");
    }
    if total_requests > 0 {
        s.push_str(&format!(
            "- Avg tokens per request: {:.1}\n",
            total_tokens as f64 / total_requests as f64
        ));
        s.push_str(&format!(
            "- Avg cost per request: ${:.6}\n",
            total_cost / total_requests as f64
        ));
    } else {
        s.push_str("- Avg tokens per request: N/A (0 requests in this run)\n");
        s.push_str("- Avg cost per request: N/A (0 requests in this run)\n");
    }
    s.push_str(&format!("- Estimated total cost: ${total_cost:.6}\n"));
    if any_unpriced {
        s.push_str(
            "- Note: at least one model has no pricing entry in `code/config/models.toml`; \
             its calls are counted in tokens/calls above but excluded from the cost total.\n",
        );
    }
    s.push('\n');

    s.push_str("## Per-model breakdown\n\n");
    s.push_str(
        "| Model | Provider | Calls | Cache hits | Input tokens | Output tokens | Total tokens | Avg tokens/call | Est. cost |\n",
    );
    s.push_str("|---|---|---|---|---|---|---|---|---|\n");
    if by_model.is_empty() {
        s.push_str("| — | — | 0 | 0 (—) | 0 | 0 | 0 | 0.0 | $0.000000 |\n");
    } else {
        for (model_id, m) in &by_model {
            let total = m.prompt_tokens + m.completion_tokens;
            let avg_per_call = if m.calls == 0 { 0.0 } else { total as f64 / m.calls as f64 };
            let cost_cell = if pricing.contains_key(model_id) {
                format!("${:.6}", m.cost_usd)
            } else {
                "N/A (no pricing on file)".to_string()
            };
            let hit_rate = if m.calls == 0 { 0.0 } else { 100.0 * m.cache_hits as f64 / m.calls as f64 };
            s.push_str(&format!(
                "| {model_id} | {} | {} | {} ({hit_rate:.1}%) | {} | {} | {} | {avg_per_call:.1} | {cost_cell} |\n",
                m.provider, m.calls, m.cache_hits, m.prompt_tokens, m.completion_tokens, total
            ));
        }
    }
    let overall_avg = if total_calls == 0 {
        0.0
    } else {
        total_tokens as f64 / total_calls as f64
    };
    let overall_hit_rate = if total_calls == 0 { 0.0 } else { 100.0 * total_cache_hits as f64 / total_calls as f64 };
    s.push_str(&format!(
        "| **Overall** | — | {total_calls} | {total_cache_hits} ({overall_hit_rate:.1}%) | {total_prompt} | {total_completion} | {total_tokens} | {overall_avg:.1} | ${total_cost:.6} |\n",
    ));

    s
}

/// Renders and writes `render_usage_report`'s output to `out_path`, creating
/// parent directories as needed (used for `code/evaluation/usage_report.md`).
pub fn write_usage_report(
    out_path: &Path,
    records: &[Usage],
    pricing: &std::collections::HashMap<String, Pricing>,
    total_requests: usize,
) -> Result<()> {
    let content = render_usage_report(records, pricing, total_requests);
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_call() -> ModelCall {
        ModelCall {
            model_id: "owner/Model".to_string(),
            provider: "some-provider".to_string(),
            model_revision: "abc123".to_string(),
            prompt_version: "v1".to_string(),
            system_prompt: "system".to_string(),
            user_content: vec![ContentPart::Text("hello".to_string())],
            temperature: 0.0,
            seed: 42,
            max_tokens: 100,
            json_response: true,
            json_schema: None,
        }
    }

    #[test]
    fn cache_key_is_stable_for_identical_calls() {
        let a = sample_call();
        let b = sample_call();
        assert_eq!(cache_key_for(&a), cache_key_for(&b));
    }

    #[test]
    fn cache_key_changes_with_prompt_version() {
        let a = sample_call();
        let mut b = sample_call();
        b.prompt_version = "v2".to_string();
        assert_ne!(cache_key_for(&a), cache_key_for(&b));
    }

    #[test]
    fn cache_key_changes_with_model_revision() {
        let a = sample_call();
        let mut b = sample_call();
        b.model_revision = "def456".to_string();
        assert_ne!(cache_key_for(&a), cache_key_for(&b));
    }

    #[test]
    fn cache_key_changes_with_content() {
        let a = sample_call();
        let mut b = sample_call();
        b.user_content = vec![ContentPart::Text("different".to_string())];
        assert_ne!(cache_key_for(&a), cache_key_for(&b));
    }

    /// Live smoke test against the real router (cheapest LLM candidate,
    /// deepinfra). Requires `HF_TOKEN`. Not run by default: `cargo test --
    /// --ignored hf::tests::live_smoke_test_gpt_oss_120b`.
    #[test]
    #[ignore]
    fn live_smoke_test_gpt_oss_120b() {
        let dir = std::env::temp_dir().join("buyorwait_hf_smoke_test_cache");
        let client = HfClient::with_cache_dir(&dir).expect("HF_TOKEN must be set");
        let call = ModelCall {
            model_id: "openai/gpt-oss-120b".to_string(),
            provider: "deepinfra".to_string(),
            model_revision: "b5c939de8f754692c1647ca79fbf85e8c1e70f8a".to_string(),
            prompt_version: "smoke-test-v1".to_string(),
            system_prompt: "Reply with strict JSON only: {\"ok\": true}. No other text."
                .to_string(),
            user_content: vec![ContentPart::Text("ping".to_string())],
            temperature: 0.0,
            seed: 42,
            max_tokens: 200,
            json_response: true,
            json_schema: None,
        };
        let resp = client.chat_completion(&call).expect("live call failed");
        println!("smoke test raw_text: {:?}", resp.raw_text);
        println!("smoke test usage: {:?}", resp.usage);
        assert!(!resp.usage.cache_hit);
        assert!(resp.usage.prompt_tokens > 0);
        assert!(resp.usage.completion_tokens > 0);

        // Second call must hit the cache: zero tokens, cache_hit = true.
        let resp2 = client.chat_completion(&call).expect("cached call failed");
        assert!(resp2.usage.cache_hit);
        assert_eq!(resp2.raw_text, resp.raw_text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn usage(model_id: &str, provider: &str, prompt: u64, completion: u64) -> Usage {
        Usage {
            model_id: model_id.to_string(),
            provider: provider.to_string(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            latency_ms: 100,
            cache_hit: false,
        }
    }

    #[test]
    fn usage_report_baseline_zero_calls_has_every_section() {
        let report = render_usage_report(&[], &std::collections::HashMap::new(), 250);
        assert!(report.contains("## Overview"));
        assert!(report.contains("## Per-model breakdown"));
        assert!(report.contains("Model calls: 0"));
        assert!(report.contains("Total tokens: 0"));
        assert!(report.contains("Estimated total cost: $0.000000"));
        assert!(report.contains("**Overall**"));
    }

    #[test]
    fn usage_report_aggregates_multiple_calls_per_model() {
        let records = vec![
            usage("model-a", "provider-x", 100, 50),
            usage("model-a", "provider-x", 200, 60),
            usage("model-b", "provider-y", 10, 5),
        ];
        let mut pricing = std::collections::HashMap::new();
        pricing.insert(
            "model-a".to_string(),
            Pricing { input_per_m: 1.0, output_per_m: 2.0 },
        );
        // model-b intentionally left unpriced.
        let report = render_usage_report(&records, &pricing, 2);
        assert!(report.contains("Model calls: 3"));
        assert!(report.contains("Total tokens: 425")); // (100+50)+(200+60)+(10+5)
        assert!(report.contains("N/A (no pricing on file)"));
        // model-a cost: (300/1e6)*1.0 + (110/1e6)*2.0
        let expected_cost = (300.0 / 1_000_000.0) * 1.0 + (110.0 / 1_000_000.0) * 2.0;
        assert!(report.contains(&format!("${expected_cost:.6}")));
    }
}
