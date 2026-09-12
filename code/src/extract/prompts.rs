//! Load a versioned prompt file under `code/prompts/` (owner: extraction) into its
//! `prompt_version` + system prompt + user template, for a real model call. Mirrors
//! ml-engineer's `bakeoff.rs` parsing convention so the two never drift (PLAN.md §3:
//! prompts are never hardcoded in Rust — this is the only place that reads the markdown
//! files' structure).

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

pub struct PromptSet {
    pub version: String,
    pub system_prompt: String,
    pub user_template: String,
}

/// `user_template_needle` selects which second fenced block to pull (each prompt file has
/// more than one heading with a code block; the "user prompt template" heading's text is
/// the needle).
pub fn load(path: &Path, user_template_needle: &str) -> Result<PromptSet> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(PromptSet {
        version: version(&text)?,
        system_prompt: fenced_block(&text, "System prompt")?,
        user_template: fenced_block(&text, user_template_needle)?,
    })
}

fn version(markdown: &str) -> Result<String> {
    for line in markdown.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            return Ok(rest.trim().to_string());
        }
    }
    bail!("no `# prompt_version` heading found")
}

/// First fenced code block under the first heading whose text contains `needle`.
fn fenced_block(markdown: &str, needle: &str) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_message_extraction_prompt() {
        let path = Path::new("prompts/message_extraction.v1.md");
        let prompt = load(path, "User prompt template").expect("prompt file should parse");
        assert_eq!(prompt.version, "message_extraction.v1");
        assert!(prompt.system_prompt.contains("financial-message field extractor"));
        assert!(prompt.user_template.contains("{{MESSAGES_BLOCK}}"));
    }

    #[test]
    fn loads_image_transcription_prompt() {
        let path = Path::new("prompts/image_transcription.v1.md");
        let prompt = load(path, "User prompt template").expect("prompt file should parse");
        assert_eq!(prompt.version, "image_transcription.v1");
        assert!(prompt.system_prompt.contains("transcribe every labeled numeric figure"));
    }
}
