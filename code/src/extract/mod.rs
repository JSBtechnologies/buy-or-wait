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
    if let Ok(v) = serde_json::from_str(unfenced) {
        return Ok(v);
    }
    // A thinking model (e.g. Kimi-K3, ml-engineer #204) can still prefix its reply with
    // reasoning text even outside a ```-fence; locate the outermost {...} or [...] and
    // parse just that, rather than requiring the whole reply to be JSON.
    if let Some(json_slice) = extract_json_span(unfenced) {
        if let Ok(v) = serde_json::from_str(json_slice) {
            return Ok(v);
        }
    }
    Err(anyhow::anyhow!("model reply is not valid JSON\nraw: {trimmed}"))
}

/// The substring from the first `{`/`[` to the matching last `}`/`]`, tracking string
/// literals so a brace inside quoted text doesn't end the span early. `None` if no opening
/// bracket is found.
fn extract_json_span(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{' || b == b'[')?;
    let opening = bytes[start];
    let closing = if opening == b'{' { b'}' } else { b']' };
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b if b == opening => depth += 1,
            b if b == closing => {
                depth -= 1;
                if depth == 0 {
                    return text.get(start..=i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_with_reasoning_preamble() {
        let reply = "Let me think about this step by step. The subtotal is 100 and tax is 5.\n\n{\"subtotal\": 100.0, \"tax\": 5.0}";
        let v = parse_json_reply(reply).expect("should extract the JSON span");
        assert_eq!(v["subtotal"], 100.0);
    }

    #[test]
    fn extract_json_span_ignores_braces_inside_strings() {
        let text = r#"noise {"a": "text with } inside", "b": 2} trailing"#;
        let span = extract_json_span(text).unwrap();
        assert_eq!(span, r#"{"a": "text with } inside", "b": 2}"#);
    }
}
