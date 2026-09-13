//! Anthropic (Claude) API client (owner: ml-engineer). Board decision
//! `decision.claude_backup`: a backup model path that needs only
//! `ANTHROPIC_API_KEY`, not HF Inference Providers credits — built after the
//! HF router account's monthly credits were exhausted mid-bake-off (see
//! `docs/bakeoff.md` "Backup models").
//!
//! Exposes the **same call shape** as `crate::hf::HfClient`: `ModelCall` in,
//! `ModelResponse` out, same `sha256(content + model_id + model_revision +
//! prompt_version)` cache key (`crate::hf::cache_key_for`), same disk cache
//! layout, same connect/request timeout + no-connection-reuse + circuit-
//! breaker-friendly retry classification as `hf.rs`. Extraction dispatches by
//! `candidate.provider == "anthropic"` vs everything else without needing to
//! know which backend actually served the call.
//!
//! Spec (verified against the Anthropic Messages API docs):
//! - `POST https://api.anthropic.com/v1/messages`
//! - Headers: `x-api-key: $ANTHROPIC_API_KEY` (env only, never logged),
//!   `anthropic-version: 2023-06-01`, `content-type: application/json`
//! - Never send `temperature`/`top_p`/`top_k` — Opus 5 returns 400 if any of
//!   these are present.
//! - `stop_reason` is checked **before** attempting to parse content:
//!   `"refusal"` or `"max_tokens"` means the read failed; the caller must
//!   never guess a value out of a truncated or refused response.
//! - The answer is the **first content block with `type: "text"`**;
//!   `"thinking"` blocks (adaptive thinking, on by default on Opus 5 and
//!   counted in `usage.output_tokens`) are never parsed as the answer.
//! - `usage.input_tokens`/`usage.output_tokens` map to `Usage`, with
//!   `provider = "anthropic"`.

use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::hf::{cache_key_for, ContentPart, ModelCall, ModelResponse, Usage};

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_VERSION: &str = "2023-06-01";

enum AnthropicCallError {
    /// HTTP 429 or 5xx, or a transport-level blip: worth retrying with backoff.
    Retryable(u16),
    /// Anything else, including a "refusal"/"max_tokens" stop_reason: never
    /// retried, and never guessed at by the caller.
    Fatal(String),
}

impl fmt::Display for AnthropicCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnthropicCallError::Retryable(status) => write!(f, "retryable HTTP {status}"),
            AnthropicCallError::Fatal(msg) => write!(f, "{msg}"),
        }
    }
}

pub struct AnthropicClient {
    http: reqwest::blocking::Client,
    api_key: String,
    cache_dir: PathBuf,
    max_attempts: u32,
    backoff_base_ms: u64,
    backoff_multiplier: f64,
    backoff_max_ms: u64,
    usage_log: Mutex<Vec<Usage>>,
}

impl AnthropicClient {
    /// Reads `ANTHROPIC_API_KEY` from the environment. Caches under
    /// `store/model_cache` by default — the same directory `HfClient::new()`
    /// uses, since both are content-addressed by `cache_key_for` and never
    /// collide (the key folds in `model_id`).
    pub fn new() -> Result<Self> {
        Self::with_cache_dir("store/model_cache")
    }

    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Result<Self> {
        let api_key = env::var(ANTHROPIC_API_KEY_ENV)
            .with_context(|| format!("{ANTHROPIC_API_KEY_ENV} env var not set"))?;
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            // Same hardening as hf.rs: a stale pooled keep-alive connection
            // caused multi-minute hangs past the configured timeout during
            // the HF bake-off runs. Force a fresh connection per request.
            .pool_max_idle_per_host(0)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            api_key,
            cache_dir: cache_dir.into(),
            max_attempts: 5,
            backoff_base_ms: 500,
            backoff_multiplier: 2.0,
            backoff_max_ms: 8000,
            usage_log: Mutex::new(Vec::new()),
        })
    }

    pub fn with_request_timeout(mut self, secs: u64) -> Result<Self> {
        self.http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs((secs / 4).max(5)))
            .timeout(Duration::from_secs(secs))
            .pool_max_idle_per_host(0)
            .build()
            .context("failed to rebuild HTTP client with custom timeout")?;
        Ok(self)
    }

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

    pub fn usage_records(&self) -> Vec<Usage> {
        self.usage_log.lock().expect("usage_log mutex poisoned").clone()
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        self.cache_dir.join(format!("{key}.json"))
    }

    fn cache_path_run(&self, key: &str, run_idx: u32) -> PathBuf {
        self.cache_dir.join(format!("{key}__run{run_idx}.json"))
    }

    fn record_usage(&self, usage: &Usage) {
        self.usage_log.lock().expect("usage_log mutex poisoned").push(usage.clone());
    }

    fn write_cache(&self, path: &PathBuf, response: &ModelResponse) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(serialized) = serde_json::to_vec_pretty(response) {
            let _ = fs::write(path, serialized);
        }
    }

    /// Cache-first (same §2.11 key as every other backend).
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

    /// Never reads the cache (still writes to it) — for `--cold` runs and
    /// bake-off stability sampling, same as `HfClient::chat_completion_cold`.
    pub fn chat_completion_cold(&self, call: &ModelCall) -> Result<ModelResponse> {
        let response = self.call_with_retry(call)?;
        let key = cache_key_for(call);
        self.write_cache(&self.cache_path(&key), &response);
        self.record_usage(&response.usage);
        Ok(response)
    }

    /// Same as `chat_completion_cold`, but also persists this run's raw
    /// response under its own `{key}__run{run_idx}.json` file so an N-run
    /// stability sweep (same cache key every run, by construction) doesn't
    /// lose runs 1..N-1 when run N overwrites the canonical cache file.
    /// Mirrors `HfClient::chat_completion_cold_numbered` (analyst request,
    /// bus topic `blocker`, RULES.md#S5).
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

    fn call_with_retry(&self, call: &ModelCall) -> Result<ModelResponse> {
        let mut attempt = 0u32;
        let mut backoff_ms = self.backoff_base_ms;
        loop {
            attempt += 1;
            match self.call_once(call) {
                Ok(resp) => return Ok(resp),
                Err(AnthropicCallError::Retryable(status)) if attempt < self.max_attempts => {
                    let reason = if status == 0 {
                        "network/transport error".to_string()
                    } else {
                        format!("retryable HTTP {status}")
                    };
                    eprintln!(
                        "anthropic: {reason} calling {} (attempt {attempt}/{}); backing off {backoff_ms}ms",
                        call.model_id, self.max_attempts
                    );
                    thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = (((backoff_ms as f64) * self.backoff_multiplier) as u64).min(self.backoff_max_ms);
                }
                Err(err) => bail!("anthropic: call to {} failed after {attempt} attempt(s): {err}", call.model_id),
            }
        }
    }

    fn call_once(&self, call: &ModelCall) -> Result<ModelResponse, AnthropicCallError> {
        let mut content: Vec<Value> = Vec::new();
        for part in &call.user_content {
            match part {
                ContentPart::ImageDataUrl { mime, base64_data } => {
                    content.push(json!({
                        "type": "image",
                        "source": { "type": "base64", "media_type": mime, "data": base64_data }
                    }));
                }
                ContentPart::Text(t) => {
                    content.push(json!({ "type": "text", "text": t }));
                }
            }
        }

        let body = json!({
            "model": call.model_id,
            "max_tokens": call.max_tokens,
            "system": call.system_prompt,
            "messages": [ { "role": "user", "content": content } ],
        });
        // Deliberately NOT sending temperature/top_p/top_k — Opus 5 returns
        // 400 Bad Request if any of these are present in the body.
        //
        // Also deliberately NOT sending output_config.format.json_schema
        // (lead directive, after 3 live schema-validation errors in a row —
        // type-array+enum rejection, the 16-union-field cap, then "Schema is
        // too complex" even after both fixes): relies on the prompt text
        // alone to ask for JSON, the same path every HF-router model in this
        // project already uses. The lenient parser (`parse_json_reply`) plus
        // reconciliation/grounding/agreement already validate every read
        // regardless of provider, so one consistent parse path across
        // providers is simpler than a second structured-output contract.

        let started = Instant::now();
        let response = self
            .http
            .post(ANTHROPIC_MESSAGES_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| {
                if e.is_timeout() || e.is_connect() || e.is_request() {
                    AnthropicCallError::Retryable(0)
                } else {
                    AnthropicCallError::Fatal(format!("request failed: {e}"))
                }
            })?;

        let status = response.status();
        if status.as_u16() == 429 || status.is_server_error() {
            return Err(AnthropicCallError::Retryable(status.as_u16()));
        }
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            return Err(AnthropicCallError::Fatal(format!("HTTP {status}: {text}")));
        }

        let parsed: MessagesResponse = response
            .json()
            .map_err(|e| AnthropicCallError::Fatal(format!("failed to parse response JSON: {e}")))?;
        let latency_ms = started.elapsed().as_millis() as u64;

        // stop_reason checked BEFORE attempting to parse content: a refusal
        // or truncated (max_tokens) response must never be guessed at.
        if parsed.stop_reason == "refusal" || parsed.stop_reason == "max_tokens" {
            return Err(AnthropicCallError::Fatal(format!(
                "failed read: stop_reason={:?} (never guessing a value from a {} response)",
                parsed.stop_reason, parsed.stop_reason
            )));
        }

        // The answer is the first "text" content block; "thinking" blocks
        // (adaptive thinking, on by default on Opus 5) are never the answer.
        let text = parsed
            .content
            .iter()
            .find(|b| b.block_type == "text")
            .and_then(|b| b.text.clone())
            .ok_or_else(|| AnthropicCallError::Fatal("no text content block in response".to_string()))?;

        Ok(ModelResponse {
            raw_text: text,
            usage: Usage {
                model_id: call.model_id.clone(),
                provider: "anthropic".to_string(),
                prompt_tokens: parsed.usage.input_tokens,
                completion_tokens: parsed.usage.output_tokens,
                latency_ms,
                cache_hit: false,
            },
        })
    }
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: String,
    usage: MessagesUsage,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessagesUsage {
    input_tokens: u64,
    output_tokens: u64,
}

// `image_figures_json_schema()` was deleted here (lead directive, extraction
// #281): it built a strict JSON Schema for Anthropic's `output_config.
// format.json_schema`, which this client no longer sends at all (dropped
// after 3 live schema-validation errors in a row — see `call_once` above;
// Claude now uses the same prompt-only JSON path as every HF-router
// candidate). A stale v1-shaped schema (amount_due_before_date/_value,
// amount_due_after_date — superseded by prompt v2's due_cutoff_date/
// amount_due_by_cutoff/amount_due_after_cutoff) sitting around unused is a
// trap for whoever re-enables structured output later; deleted rather than
// updated to v2, since nothing calls it.
