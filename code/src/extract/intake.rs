//! Interactive-mode request intake (PLAN.md §2.6, schema =
//! `code/prompts/request_text_extraction.v1.md`). Batch mode (this submission) does not use
//! this module: `engine::types::RequestSpec::from_model` reads the four columns straight
//! from `requests.csv`, 0 tokens. Both paths build the same `RequestSpec`, so the engine
//! never knows which mode fed it.

use serde::Deserialize;

use crate::engine::money::Money;
use crate::engine::types::RequestSpec;
use crate::extract::model_config::{CandidateConfig, DecodingConfig};
use crate::extract::parse_json_reply;
use crate::extract::prompts::PromptSet;
use crate::hf::{ContentPart, HfClient, ModelCall};

#[derive(Debug, Clone, Deserialize)]
struct RequestTextFields {
    #[serde(default)]
    amount: Option<f64>,
    #[serde(default)]
    deadline: Option<String>,
    #[serde(default, rename = "type")]
    request_type: Option<String>,
    #[serde(default)]
    allows_partial_payment: Option<bool>,
}

/// Parse free text into the same struct batch mode builds from CSV columns. Returns `None`
/// when any required field (amount, deadline, type) could not be grounded in the text —
/// this module never guesses a missing field into a decision-affecting default. Goes
/// through `HfClient`'s own §2.11 disk cache. Uses the same `llm_primary` candidate as the
/// message path (`code/config/models.toml`'s `[selected]` table).
pub fn parse_request_text(
    client: &HfClient,
    cold: bool,
    prompt: &PromptSet,
    decoding: &DecodingConfig,
    candidate: &CandidateConfig,
    request_text: &str,
) -> anyhow::Result<Option<RequestSpec>> {
    let user_content = prompt.user_template.replace("{{REQUEST_TEXT}}", request_text);
    let call = ModelCall {
        model_id: candidate.id.clone(),
        provider: candidate.provider.clone(),
        model_revision: candidate.model_revision.clone(),
        prompt_version: prompt.version.clone(),
        system_prompt: prompt.system_prompt.clone(),
        user_content: vec![ContentPart::Text(user_content)],
        temperature: decoding.temperature,
        seed: decoding.seed,
        max_tokens: decoding.max_tokens_llm,
        json_response: candidate.supports_structured_output,
    };
    let response =
        if cold { client.chat_completion_cold(&call)? } else { client.chat_completion(&call)? };
    let value = parse_json_reply(&response.raw_text)?;
    let fields: RequestTextFields = serde_json::from_value(value)?;

    let spec = (|| {
        let amount = fields.amount?;
        let deadline = chrono::NaiveDate::parse_from_str(fields.deadline.as_deref()?, "%Y-%m-%d").ok()?;
        let request_type = fields.request_type?;
        Some(RequestSpec {
            amount: Money::from_f64(amount),
            deadline,
            request_type,
            allows_partial_payment: fields.allows_partial_payment.unwrap_or(false),
        })
    })();

    Ok(spec)
}
