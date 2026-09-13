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
        json_schema: None,
    };
    let response =
        if cold { client.chat_completion_cold(&call)? } else { client.chat_completion(&call)? };
    let value = parse_json_reply(&response.raw_text)?;
    let fields: RequestTextFields = serde_json::from_value(value)?;
    let deadline_ok = fields.deadline.as_deref().is_some_and(|d| parse_deadline(d).is_some());
    if fields.amount.is_none() || !deadline_ok || fields.request_type.is_none() {
        eprintln!(
            "intake: ungrounded fields -- amount={:?} deadline={:?} type={:?}",
            fields.amount, fields.deadline, fields.request_type
        );
    }

    let spec = (|| {
        let amount = fields.amount?;
        let deadline = parse_deadline(fields.deadline.as_deref()?)?;
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

/// Deadline as the model returned it: ISO `YYYY-MM-DD` first, then an unambiguous written date
/// ("13 February 2023", "13 Feb 2023", "February 13, 2023"). Numeric day/month forms are not
/// guessed here -- an ambiguous date stays ungrounded.
fn parse_deadline(raw: &str) -> Option<chrono::NaiveDate> {
    let s = raw.trim();
    ["%Y-%m-%d", "%d %B %Y", "%d %b %Y", "%B %d, %Y", "%b %d, %Y", "%B %d %Y", "%d-%b-%Y"]
        .iter()
        .find_map(|f| chrono::NaiveDate::parse_from_str(s, f).ok())
}

#[cfg(test)]
mod deadline_tests {
    use super::parse_deadline;
    use chrono::NaiveDate;

    #[test]
    fn written_and_iso_deadlines_parse_but_numeric_day_month_does_not() {
        let d = NaiveDate::from_ymd_opt(2023, 2, 13);
        assert_eq!(parse_deadline("2023-02-13"), d);
        assert_eq!(parse_deadline("13 February 2023"), d);
        assert_eq!(parse_deadline("13 Feb 2023"), d);
        assert_eq!(parse_deadline("February 13, 2023"), d);
        assert_eq!(parse_deadline("13/02/2023"), None);
    }
}
