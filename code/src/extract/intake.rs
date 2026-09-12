//! Interactive-mode request intake (PLAN.md §2.6, schema =
//! `code/prompts/request_text_extraction.v1.md`). Batch mode (this submission) does not use
//! this module: `engine::types::RequestSpec::from_model` reads the four columns straight
//! from `requests.csv`, 0 tokens. Both paths build the same `RequestSpec`, so the engine
//! never knows which mode fed it.

use serde::Deserialize;

use crate::engine::money::Cents;
use crate::engine::types::RequestSpec;
use crate::extract::{parse_json_reply, ModelClient};

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
/// this module never guesses a missing field into a decision-affecting default.
pub fn parse_request_text(
    client: &dyn ModelClient,
    system_prompt: &str,
    request_text: &str,
) -> anyhow::Result<(Option<RequestSpec>, u32, u32)> {
    let user_prompt = format!(
        "Extract the four fields from this request. Respond with the JSON object only.\n\n{request_text}"
    );
    let response = client.complete(system_prompt, &user_prompt, &[])?;
    let value = parse_json_reply(&response.text)?;
    let fields: RequestTextFields = serde_json::from_value(value)?;

    let spec = (|| {
        let amount = fields.amount?;
        let deadline = chrono::NaiveDate::parse_from_str(fields.deadline.as_deref()?, "%Y-%m-%d").ok()?;
        let request_type = fields.request_type?;
        Some(RequestSpec {
            amount: Cents::from_f64(amount),
            deadline,
            request_type,
            allows_partial_payment: fields.allows_partial_payment.unwrap_or(false),
        })
    })();

    Ok((spec, response.prompt_tokens, response.completion_tokens))
}
