//! Integration test (owner: extraction): `code/src/bin/gen_evidence.rs`'s output and a
//! direct call to `extract::messages::deterministic_evidence` must never disagree for any
//! of the 275 users — they are the two places PLAN.md §2.11's evidence is produced, and a
//! silent divergence between them (lead: engine #172, message_55/user_73) is exactly the
//! kind of bug invariants exist to catch mechanically instead of by inspection.
//!
//! Run from `code/` with `cargo test --test evidence_consistency`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use buyorwait::extract::{messages::deterministic_evidence, retrieval};
use buyorwait::model;

/// Rebuilds exactly what `gen_evidence.rs` computes for every user: the same `as_of`
/// resolution (sample_requests.csv for users 01-25, requests.csv otherwise) and the same
/// `retrieval::for_user` + `deterministic_evidence` call.
fn evidence_by_user() -> BTreeMap<String, String> {
    let dataset_dir = Path::new("../dataset");
    let profiles = model::load_financial_profiles(dataset_dir.join("financial_profiles.csv"))
        .expect("financial_profiles.csv should load");
    let messages =
        model::load_messages(dataset_dir.join("messages.csv")).expect("messages.csv should load");
    let images: Vec<model::Image> = Vec::new();
    let sample_requests = model::load_sample_requests(dataset_dir.join("sample_requests.csv"))
        .expect("sample_requests.csv should load");
    let requests =
        model::load_requests(dataset_dir.join("requests.csv")).expect("requests.csv should load");

    let mut as_of_by_user: HashMap<String, chrono::NaiveDate> = HashMap::new();
    for r in &sample_requests {
        as_of_by_user.insert(r.user_id.clone(), r.request_date);
    }
    for r in &requests {
        as_of_by_user.entry(r.user_id.clone()).or_insert(r.request_date);
    }

    let mut out = BTreeMap::new();
    for (user_id, &as_of) in &as_of_by_user {
        let Some(profile) = profiles.iter().find(|p| &p.user_id == user_id) else { continue };
        let index = retrieval::for_user(user_id, as_of, &messages, &images);
        let evidence = deterministic_evidence(&index.messages, &profile.home_currency);
        let json = serde_json::to_string(&evidence).expect("evidence should serialize");
        out.insert(user_id.clone(), json);
    }
    out
}

/// `gen_evidence.rs`'s exact procedure, run twice independently, must produce byte-identical
/// serialized evidence for all 275 users — the same guarantee PLAN.md §2.11 requires of a
/// warm rerun producing a byte-identical `output.csv`. Catches any hidden non-determinism
/// (HashMap iteration order, uninitialized state) as well as a straight code-path
/// divergence between the two callers of `deterministic_evidence`.
#[test]
fn gen_evidence_and_deterministic_evidence_agree_for_all_users() {
    let first = evidence_by_user();
    let second = evidence_by_user();
    assert_eq!(first.len(), 275, "expected evidence computed for all 275 users");
    assert_eq!(first, second, "two independent runs of the same evidence path diverged");
}

/// Explicit regression for lead: engine #172 — every A5 rent-renewal message (12, 51, 55,
/// 61, 105, 147, 175; EN + ID) must produce a Fact::ExpenseAmountChange for its user via
/// the live `deterministic_evidence` path, the same one `gen_evidence.rs` and
/// `main.rs::decide_one` both call.
#[test]
fn every_a5_rent_message_reaches_the_live_path() {
    let by_user = evidence_by_user();
    let a5_users = [
        ("message_12", "user_16"),
        ("message_51", "user_69"),
        ("message_55", "user_73"),
        ("message_61", "user_81"),
        ("message_105", "user_137"),
        ("message_147", "user_185"),
        ("message_175", "user_225"),
    ];
    for (message_id, user_id) in a5_users {
        let json = by_user
            .get(user_id)
            .unwrap_or_else(|| panic!("no evidence computed for {user_id} ({message_id})"));
        assert!(
            json.contains("ExpenseAmountChange") && json.contains(message_id),
            "{user_id} ({message_id}) missing ExpenseAmountChange in live evidence: {json}"
        );
    }
}
