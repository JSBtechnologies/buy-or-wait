//! LLM generalization test (owner: ml-engineer, lead-requested step 5, 2026-09-13).
//!
//! Runs the candidate LLM (SEA-LION-v4-27B, DeepSeek-V4-Pro-0813 fallback) through
//! `extract::messages::extract_batch` on messages with `parse_known_skeleton`
//! DELIBERATELY BYPASSED -- i.e. the LLM sees and answers every message, not only
//! the ones the zero-token deterministic parser cannot already resolve. This is a
//! generalization stress test, not a production path: production never calls the
//! LLM on a skeleton-resolved message (PLAN.md §3 batching lever).
//!
//! Two data sources:
//! (a) `dataset/messages.csv` (215 real messages) -- diffed per-message against
//!     `extract::messages::deterministic_evidence` (analyst-audited correct) for
//!     every message the deterministic parser DOES resolve, classified
//!     agree/missing/extra/wrong.
//! (b) extraction's 20 synthetic EN+ID paraphrase fixtures
//!     (`docs/llm_generalization_fixtures.json`), constructed so the skeleton
//!     regexes never match -- checked against each fixture's own
//!     `expected_llm_record`/`expected_facts`, plus that grounding rejects any
//!     claim not literally present in the fixture's text.
//!
//! Writes a markdown report; does not touch production code paths.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use buyorwait::extract::grounding::amount_grounded;
use buyorwait::extract::messages::{deterministic_evidence, extract_batch, skeleton, to_evidence, MessageRecord};
use buyorwait::extract::model_config::{CandidateConfig, DecodingConfig};
use buyorwait::extract::prompts::{self, PromptSet};
use buyorwait::hf::HfClient;
use buyorwait::model::{load_financial_profiles, load_messages, Message};
use serde::Deserialize;
use serde_json::Value;

const RUNS: u32 = 3;
const BATCH_SIZE: usize = 40;

fn sea_lion() -> CandidateConfig {
    CandidateConfig {
        id: "aisingapore/Gemma-SEA-LION-v4-27B-IT".to_string(),
        provider: "publicai".to_string(),
        model_revision: "c41c291558bdd8cbc37d77868cec0bc5b6921476".to_string(),
        supports_structured_output: true,
        role: None,
    }
}

fn deepseek_fallback() -> CandidateConfig {
    CandidateConfig {
        id: "deepseek-ai/DeepSeek-V4-Pro-0813".to_string(),
        provider: "deepinfra".to_string(),
        model_revision: "72e1d3230f6c080a530b0a1d46f8eb4602340597".to_string(),
        supports_structured_output: true,
        role: Some("llm".to_string()),
    }
}

fn decoding() -> DecodingConfig {
    DecodingConfig { temperature: 0.0, seed: 42, max_tokens_vlm: 1, max_tokens_llm: 4000 }
}

#[derive(Debug, Deserialize)]
struct FixtureFile {
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    id: String,
    language: String,
    #[allow(dead_code)]
    topic: String,
    text: String,
    expected_llm_record: Value,
    #[allow(dead_code)]
    expected_facts: Value,
}

#[derive(Debug, Default)]
struct DiffCounts {
    agree: u32,
    missing: u32,
    extra: u32,
    wrong: u32,
    no_fact_either_side: u32,
}

fn classify(det: &[buyorwait::engine::ledger::Fact], llm: &[buyorwait::engine::ledger::Fact]) -> &'static str {
    let det_empty = det.is_empty();
    let llm_empty = llm.is_empty();
    if det_empty && llm_empty {
        return "no_fact_either_side";
    }
    if !det_empty && llm_empty {
        return "missing";
    }
    if det_empty && !llm_empty {
        return "extra";
    }
    // Both non-empty: order-independent set match.
    let same_len = det.len() == llm.len();
    let all_found = det.iter().all(|d| llm.contains(d));
    if same_len && all_found {
        "agree"
    } else {
        "wrong"
    }
}

fn call_with_fallback(
    client: &HfClient,
    prompt: &PromptSet,
    dec: &DecodingConfig,
    primary: &CandidateConfig,
    fallback: &CandidateConfig,
    batch: &[&Message],
) -> Result<HashMap<String, Vec<MessageRecord>>> {
    match extract_batch(client, true, prompt, dec, primary, batch) {
        Ok(by_id) => Ok(by_id),
        Err(e) => {
            eprintln!("llm_generalization: primary {} failed ({e:#}), trying fallback {}", primary.id, fallback.id);
            extract_batch(client, true, prompt, dec, fallback, batch)
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let get = |flag: &str, default: &str| -> String {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
    };
    let dataset_dir = PathBuf::from(get("--dataset-dir", "../dataset"));
    let prompts_dir = PathBuf::from(get("--prompts-dir", "prompts"));
    let fixtures_path = PathBuf::from(get("--fixtures", "../docs/llm_generalization_fixtures.json"));
    let out_path = PathBuf::from(get("--out", "../docs/llm_generalization_report.md"));
    let cache_dir = PathBuf::from(get("--cache-dir", "store/bakeoff_cache_v2"));

    let messages = load_messages(dataset_dir.join("messages.csv")).context("loading messages.csv")?;
    let profiles = load_financial_profiles(dataset_dir.join("financial_profiles.csv")).context("loading financial_profiles.csv")?;
    let home_currency: HashMap<String, String> =
        profiles.into_iter().map(|p| (p.user_id, p.home_currency)).collect();

    let prompt = prompts::load(&prompts_dir.join("message_extraction.v1.md"), "User prompt template")?;
    let dec = decoding();
    let primary = sea_lion();
    let fallback = deepseek_fallback();
    let client = HfClient::with_cache_dir(&cache_dir)?.with_request_timeout(90)?.with_retry_policy(2, 1000, 2.0, 4000);

    eprintln!(
        "llm_generalization: {} real messages, skeleton parser BYPASSED for all of them, {RUNS} runs, batch size {BATCH_SIZE}",
        messages.len()
    );

    // Ground truth: deterministic_evidence per-message (own user's home currency),
    // computed once -- this never calls a model, it is the audited baseline.
    let mut det_facts: HashMap<String, Vec<buyorwait::engine::ledger::Fact>> = HashMap::new();
    for m in &messages {
        let cur = home_currency.get(&m.user_id).cloned().unwrap_or_default();
        let ev = deterministic_evidence(&[m], &cur);
        det_facts.insert(m.message_id.clone(), ev.into_iter().map(|e| e.fact).collect());
    }

    // Per-run LLM facts (bypassing the skeleton filter -- extract_batch takes
    // whatever slice it is given, never itself checks parse_known_skeleton).
    let refs: Vec<&Message> = messages.iter().collect();
    let mut per_run_llm_facts: Vec<HashMap<String, Vec<buyorwait::engine::ledger::Fact>>> = Vec::new();
    let mut total_prompt_tokens = 0u64;
    let mut total_completion_tokens = 0u64;
    let mut total_calls = 0u64;

    for run in 1..=RUNS {
        let mut this_run: HashMap<String, Vec<buyorwait::engine::ledger::Fact>> = HashMap::new();
        for chunk in refs.chunks(BATCH_SIZE) {
            eprintln!("llm_generalization: run {run}/{RUNS}, batch of {} messages ...", chunk.len());
            let by_id = call_with_fallback(&client, &prompt, &dec, &primary, &fallback, chunk)?;
            for m in chunk {
                let cur = home_currency.get(&m.user_id).cloned().unwrap_or_default();
                let mut facts = Vec::new();
                if let Some(records) = by_id.get(&m.message_id) {
                    for (idx, record) in records.iter().enumerate() {
                        if let Some(amount) = record.amount {
                            if !amount_grounded(amount, &m.message_text) {
                                eprintln!(
                                    "llm_generalization: grounding rejected {} record {idx}: claimed amount {amount} not in source text",
                                    m.message_id
                                );
                                continue;
                            }
                        }
                        if let Some(evidence) = to_evidence(m, idx, record, &cur) {
                            facts.push(evidence.fact);
                        }
                    }
                }
                this_run.insert(m.message_id.clone(), facts);
            }
        }
        for u in client.usage_records() {
            total_calls += 1;
            total_prompt_tokens += u.prompt_tokens;
            total_completion_tokens += u.completion_tokens;
        }
        per_run_llm_facts.push(this_run);
    }

    // Diff per message across the LAST run (a single representative pass);
    // also record whether the verdict was stable across all N runs.
    let mut counts = DiffCounts::default();
    let mut by_family: HashMap<String, DiffCounts> = HashMap::new();
    let mut wrong_examples: Vec<(String, String)> = Vec::new();
    let mut stable_verdicts = 0usize;
    let mut scored = 0usize;

    for m in &messages {
        let det = det_facts.get(&m.message_id).cloned().unwrap_or_default();
        let last_llm = per_run_llm_facts.last().unwrap().get(&m.message_id).cloned().unwrap_or_default();
        let verdict = classify(&det, &last_llm);
        let family = skeleton(&m.message_text);
        let entry = by_family.entry(family).or_default();
        match verdict {
            "agree" => {
                counts.agree += 1;
                entry.agree += 1;
            }
            "missing" => {
                counts.missing += 1;
                entry.missing += 1;
            }
            "extra" => {
                counts.extra += 1;
                entry.extra += 1;
            }
            "wrong" => {
                counts.wrong += 1;
                entry.wrong += 1;
                if wrong_examples.len() < 10 {
                    wrong_examples.push((m.message_id.clone(), format!("det={det:?} llm={last_llm:?}")));
                }
            }
            _ => {
                counts.no_fact_either_side += 1;
                entry.no_fact_either_side += 1;
            }
        }
        if !det.is_empty() {
            scored += 1;
            let all_same = per_run_llm_facts
                .iter()
                .all(|run| classify(&det, run.get(&m.message_id).map(|v| v.as_slice()).unwrap_or(&[])) == verdict);
            if all_same {
                stable_verdicts += 1;
            }
        }
    }

    // (b) Extraction's 20 synthetic paraphrase fixtures.
    let fixtures_text = std::fs::read_to_string(&fixtures_path)
        .with_context(|| format!("reading {}", fixtures_path.display()))?;
    let fixtures: FixtureFile = serde_json::from_str(&fixtures_text)?;
    let mut fixture_rows = Vec::new();
    for f in &fixtures.fixtures {
        let synth_msg = Message {
            message_id: f.id.clone(),
            user_id: "synthetic".to_string(),
            request_id: None,
            related_event_id: None,
            sent_at: chrono::Utc::now(),
            source_type: "synthetic".to_string(),
            message_text: f.text.clone(),
        };
        let by_id = call_with_fallback(&client, &prompt, &dec, &primary, &fallback, &[&synth_msg])?;
        let records = by_id.get(&f.id).cloned().unwrap_or_default();
        let record_type_match = records
            .first()
            .map(|r| format!("{:?}", r.record_type).to_lowercase().contains(
                f.expected_llm_record.get("record_type").and_then(|v| v.as_str()).unwrap_or("").replace('_', "").as_str().trim()
            ))
            .unwrap_or(false);
        let amount_match = match (records.first().and_then(|r| r.amount), f.expected_llm_record.get("amount").and_then(|v| v.as_f64())) {
            (Some(a), Some(e)) => (a - e).abs() < 0.01,
            (None, None) => true,
            _ => false,
        };
        let mut grounding_ok = true;
        for r in &records {
            if let Some(a) = r.amount {
                if !amount_grounded(a, &f.text) {
                    grounding_ok = false;
                }
            }
        }
        fixture_rows.push((f.id.clone(), f.language.clone(), record_type_match, amount_match, grounding_ok, records.len()));
        for u in client.usage_records().iter().skip(total_calls as usize) {
            total_calls += 1;
            total_prompt_tokens += u.prompt_tokens;
            total_completion_tokens += u.completion_tokens;
        }
    }

    // --- report ---
    let mut s = String::new();
    s.push_str("# Step 5 -- LLM generalization test\n\n");
    s.push_str(&format!(
        "Lead-requested (2026-09-13): {} candidate = `{}` (fallback `{}`), skeleton parser \
         DELIBERATELY BYPASSED for every message so the LLM path is exercised on the whole \
         215-message set, not just the ~1 message (`msg_86`) production actually sends it. \
         N={RUNS} runs, batch size {BATCH_SIZE}. Diffed per-message against \
         `extract::messages::deterministic_evidence` (analyst-audited correct) wherever that \
         function resolves a message; a message with zero deterministic facts has no ground \
         truth here and is only counted informationally.\n\n",
        "SEA-LION-v4-27B", primary.id, fallback.id
    ));
    s.push_str("## (a) Real messages (dataset/messages.csv, 215)\n\n");
    s.push_str(&format!(
        "| Verdict | Count | Share of {} scored (deterministic non-empty) |\n|---|---|---|\n",
        scored
    ));
    let pct = |n: u32| if scored == 0 { 0.0 } else { 100.0 * n as f64 / scored as f64 };
    s.push_str(&format!("| agree | {} | {:.1}% |\n", counts.agree, pct(counts.agree)));
    s.push_str(&format!("| missing (LLM missed a fact the deterministic parser found) | {} | {:.1}% |\n", counts.missing, pct(counts.missing)));
    s.push_str(&format!("| wrong (LLM produced a different fact) | {} | {:.1}% |\n", counts.wrong, pct(counts.wrong)));
    s.push_str(&format!("| extra (LLM produced a fact where deterministic found none) | {} | (informational; deterministic had nothing to compare) |\n", counts.extra));
    s.push_str(&format!("| no_fact_either_side | {} | (informational) |\n", counts.no_fact_either_side));
    s.push_str(&format!(
        "\n**Verdict stability across all {RUNS} runs (for the {scored} messages with a deterministic answer):** {stable_verdicts}/{scored} ({:.1}%).\n\n",
        if scored == 0 { 0.0 } else { 100.0 * stable_verdicts as f64 / scored as f64 }
    ));

    s.push_str("### Per message-family breakdown\n\n");
    s.push_str("| Family (masked skeleton) | agree | missing | wrong | extra | no_fact |\n|---|---|---|---|---|---|\n");
    let mut families: Vec<_> = by_family.iter().collect();
    families.sort_by(|a, b| b.1.agree.cmp(&a.1.agree));
    for (fam, c) in families {
        let short = if fam.len() > 90 { format!("{}...", &fam[..90]) } else { fam.clone() };
        s.push_str(&format!("| `{short}` | {} | {} | {} | {} | {} |\n", c.agree, c.missing, c.wrong, c.extra, c.no_fact_either_side));
    }

    if !wrong_examples.is_empty() {
        s.push_str("\n### Sample \"wrong\" diffs (up to 10)\n\n");
        for (id, detail) in &wrong_examples {
            s.push_str(&format!("- `{id}`: {detail}\n"));
        }
    }

    s.push_str("\n## (b) Extraction's 20 synthetic paraphrase fixtures (unseen phrasings)\n\n");
    s.push_str("| Fixture | Language | record_type match | amount match | grounding OK | records extracted |\n|---|---|---|---|---|---|\n");
    let (mut rt_ok, mut amt_ok, mut ground_ok) = (0, 0, 0);
    for (id, lang, rt, amt, ground, n) in &fixture_rows {
        if *rt { rt_ok += 1; }
        if *amt { amt_ok += 1; }
        if *ground { ground_ok += 1; }
        s.push_str(&format!("| {id} | {lang} | {} | {} | {} | {n} |\n", if *rt {"yes"} else {"NO"}, if *amt {"yes"} else {"NO"}, if *ground {"yes"} else {"NO"}));
    }
    let total_f = fixture_rows.len().max(1);
    s.push_str(&format!(
        "\n**Fixture summary:** record_type match {rt_ok}/{total_f}, amount match {amt_ok}/{total_f}, grounding-OK {ground_ok}/{total_f} (0 ungrounded amounts accepted{}).\n\n",
        if ground_ok == total_f { " -- confirmed" } else { " -- SEE FAILURES ABOVE" }
    ));

    let total_tokens = total_prompt_tokens + total_completion_tokens;
    s.push_str("## Tokens and cost\n\n");
    s.push_str(&format!(
        "- Total calls (real messages {RUNS} runs + fixtures): {total_calls}\n- Input tokens: {total_prompt_tokens}\n- Output tokens: {total_completion_tokens}\n- Total tokens: {total_tokens}\n"
    ));
    // SEA-LION pricing from config/models.toml: input 0.20, output 0.40 per M tokens.
    let est_cost = (total_prompt_tokens as f64 / 1_000_000.0) * 0.20 + (total_completion_tokens as f64 / 1_000_000.0) * 0.40;
    s.push_str(&format!("- Est. cost at SEA-LION pricing ($0.20/$0.40 per M tok): ${est_cost:.6}\n"));

    std::fs::write(&out_path, &s).with_context(|| format!("writing {}", out_path.display()))?;
    eprintln!("llm_generalization: report written to {}", out_path.display());
    Ok(())
}
