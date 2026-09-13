//! One-off tool (owner: extraction): generate per-user `EvidenceRecord`s via the
//! deterministic, zero-token skeleton parser and write them to
//! `code/store/evidence/<user_id>.json` so engine can read them directly while the model
//! path (bake-off/hf.rs) is still pending — board decision `evidence_handoff`. Covers all
//! 275 users (started at 01-25, extended per lead instruction).
//!
//! Not part of the main pipeline: the integrator wires
//! `extract::messages::deterministic_evidence` into `main.rs`'s own run separately, per
//! the layering rule (engine must not depend on extract; extract depends on engine's
//! `ledger::{EvidenceRecord, Fact}` contract, not the other way around).
//!
//! Run from `code/` with `cargo run --bin gen_evidence`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use buyorwait::extract::messages::deterministic_evidence;
use buyorwait::extract::retrieval;
use buyorwait::model;

fn main() -> anyhow::Result<()> {
    let dataset_dir = Path::new("../dataset");
    let profiles = model::load_financial_profiles(dataset_dir.join("financial_profiles.csv"))?;
    let messages = model::load_messages(dataset_dir.join("messages.csv"))?;
    let images: Vec<model::Image> = Vec::new(); // not needed: only .messages is used below
    let sample_requests = model::load_sample_requests(dataset_dir.join("sample_requests.csv"))?;
    let requests = model::load_requests(dataset_dir.join("requests.csv"))?;

    // Users 01-25 live only in sample_requests.csv (requests.csv starts at user_26).
    let mut as_of_by_user: HashMap<String, chrono::NaiveDate> = HashMap::new();
    for r in &sample_requests {
        as_of_by_user.insert(r.user_id.clone(), r.request_date);
    }
    for r in &requests {
        as_of_by_user.entry(r.user_id.clone()).or_insert(r.request_date);
    }

    // Board decision.evidence_handoff: users 01-25 first, now extended to all 275 users
    // (lead: "Regenerate store/evidence for ALL 275 users").
    let mut user_ids: Vec<&String> = as_of_by_user.keys().collect();
    user_ids.sort_by_key(|u| {
        u.strip_prefix("user_").and_then(|n| n.parse::<u32>().ok()).unwrap_or(u32::MAX)
    });

    let out_dir = Path::new("store/evidence");
    fs::create_dir_all(out_dir)?;

    let mut written = 0usize;
    let mut total_facts = 0usize;
    for user_id in user_ids {
        let Some(profile) = profiles.iter().find(|p| &p.user_id == user_id) else {
            eprintln!("skip {user_id}: no financial_profiles.csv row");
            continue;
        };
        let as_of = as_of_by_user[user_id];
        let index = retrieval::for_user(user_id, as_of, &messages, &images);
        let evidence = deterministic_evidence(&index.messages, &profile.home_currency);
        total_facts += evidence.len();
        let path = out_dir.join(format!("{user_id}.json"));
        fs::write(&path, serde_json::to_string_pretty(&evidence)?)?;
        written += 1;
    }

    eprintln!(
        "wrote {written} files ({total_facts} total EvidenceRecords) to {}",
        out_dir.display()
    );
    Ok(())
}
