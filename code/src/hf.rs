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
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ROUTER_CHAT_COMPLETIONS_URL: &str = "https://router.huggingface.co/v1/chat/completions";
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

pub struct HfClient {
    http: reqwest::blocking::Client,
    token: String,
    cache_dir: PathBuf,
    max_attempts: u32,
    backoff_base_ms: u64,
    backoff_multiplier: f64,
    backoff_max_ms: u64,
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
        })
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

    /// Content hash of the call's variable inputs only. Model identity and
    /// prompt version are folded in separately by `cache_key` so identical
    /// content under a different model or prompt version gets its own entry.
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

    /// `sha256(content_hash + model_id + model_revision + prompt_version)` (PLAN.md §2.11).
    fn cache_key(call: &ModelCall) -> String {
        let content_hash = Self::content_hash(call);
        let mut hasher = Sha256::new();
        hasher.update(content_hash.as_bytes());
        hasher.update(call.model_id.as_bytes());
        hasher.update(call.model_revision.as_bytes());
        hasher.update(call.prompt_version.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        self.cache_dir.join(format!("{key}.json"))
    }

    /// Run one chat completion. Cache-first; on a miss, calls the router
    /// with retry/backoff and writes the result back to the cache.
    pub fn chat_completion(&self, call: &ModelCall) -> Result<ModelResponse> {
        let key = Self::cache_key(call);
        let path = self.cache_path(&key);

        if let Ok(bytes) = fs::read(&path) {
            if let Ok(mut cached) = serde_json::from_slice::<ModelResponse>(&bytes) {
                cached.usage.cache_hit = true;
                return Ok(cached);
            }
        }

        let response = self.call_with_retry(call)?;
        self.write_cache(&path, &response);
        Ok(response)
    }

    /// Same as `chat_completion` but never reads the cache (still writes to
    /// it). Used for `--cold` runs (PLAN.md Phase 3) so the run reflects real
    /// calls while still leaving a warm cache behind for the determinism
    /// check.
    pub fn chat_completion_cold(&self, call: &ModelCall) -> Result<ModelResponse> {
        let response = self.call_with_retry(call)?;
        let key = Self::cache_key(call);
        self.write_cache(&self.cache_path(&key), &response);
        Ok(response)
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
                    eprintln!(
                        "hf: retryable HTTP {status} calling {} via {} (attempt {attempt}/{}); backing off {backoff_ms}ms",
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
            .map_err(|e| HfCallError::Fatal(format!("request failed: {e}")))?;

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
        }
    }

    #[test]
    fn cache_key_is_stable_for_identical_calls() {
        let a = sample_call();
        let b = sample_call();
        assert_eq!(HfClient::cache_key(&a), HfClient::cache_key(&b));
    }

    #[test]
    fn cache_key_changes_with_prompt_version() {
        let a = sample_call();
        let mut b = sample_call();
        b.prompt_version = "v2".to_string();
        assert_ne!(HfClient::cache_key(&a), HfClient::cache_key(&b));
    }

    #[test]
    fn cache_key_changes_with_model_revision() {
        let a = sample_call();
        let mut b = sample_call();
        b.model_revision = "def456".to_string();
        assert_ne!(HfClient::cache_key(&a), HfClient::cache_key(&b));
    }

    #[test]
    fn cache_key_changes_with_content() {
        let a = sample_call();
        let mut b = sample_call();
        b.user_content = vec![ContentPart::Text("different".to_string())];
        assert_ne!(HfClient::cache_key(&a), HfClient::cache_key(&b));
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
}
