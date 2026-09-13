//! Verifies `docs/llm_generalization_fixtures.json` (ml-engineer step 5, lead request
//! 2026-09-12): 20 synthetic EN+ID messages, reworded from the real message families so the
//! zero-token deterministic parser never recognizes them and the LLM path is exercised. Two
//! properties are checked for every fixture:
//! 1. `parse_known_skeleton` returns `None` -- the fixture actually forces the LLM path,
//!    it isn't accidentally solvable for free.
//! 2. The fixture's own `expected_llm_record`, fed through the same `to_evidence` the LLM
//!    path calls in production, reproduces exactly the `expected_facts` (or `None`, for the
//!    own-account-transfer and prompt-injection fixtures) recorded alongside it -- so the
//!    "expected" side of this fixture set is verified against the real code, not hand-typed
//!    guesswork.
//!
//! Run from `code/` with `cargo test --test llm_generalization_fixtures`.

use std::fs;
use std::path::Path;

use buyorwait::engine::ledger::Fact;
use buyorwait::engine::money::Money;
use buyorwait::engine::types::Direction;
use buyorwait::extract::messages::{parse_known_skeleton, to_evidence, MessageRecord};
use buyorwait::model::Message;
use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::Value;

fn load_fixtures() -> Vec<Value> {
    let text = fs::read_to_string(Path::new("../docs/llm_generalization_fixtures.json"))
        .expect("docs/llm_generalization_fixtures.json should exist");
    let root: Value = serde_json::from_str(&text).expect("fixture file should be valid JSON");
    root["fixtures"].as_array().expect("fixtures should be an array").clone()
}

fn dummy_message(text: &str) -> Message {
    Message {
        message_id: "llm_gen_fixture".to_string(),
        user_id: "user_fixture".to_string(),
        request_id: None,
        related_event_id: None,
        sent_at: Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap(),
        source_type: "test_fixture".to_string(),
        message_text: text.to_string(),
    }
}

#[test]
fn every_fixture_defeats_the_deterministic_skeleton_parser() {
    let fixtures = load_fixtures();
    assert!(fixtures.len() >= 18, "expected ~20 fixtures, found {}", fixtures.len());
    for fixture in &fixtures {
        let id = fixture["id"].as_str().unwrap();
        let text = fixture["text"].as_str().unwrap();
        let result = parse_known_skeleton(text, NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
        assert!(
            result.is_none(),
            "{id} matched a known deterministic skeleton ({result:?}) -- reword it so it forces the LLM path"
        );
    }
}

#[test]
fn every_fixtures_expected_llm_record_reproduces_its_expected_facts() {
    let fixtures = load_fixtures();
    for fixture in &fixtures {
        let id = fixture["id"].as_str().unwrap();
        let text = fixture["text"].as_str().unwrap();
        let record: MessageRecord = serde_json::from_value(fixture["expected_llm_record"].clone())
            .unwrap_or_else(|e| panic!("{id}: expected_llm_record should deserialize: {e}"));
        let message = dummy_message(text);
        let expected_facts = fixture["expected_facts"].as_array().unwrap();
        let home_currency = record.currency.clone().unwrap_or_else(|| "USD".to_string());
        let evidence = to_evidence(&message, 0, &record, &home_currency);

        if expected_facts.is_empty() {
            assert!(
                evidence.is_none(),
                "{id}: expected no fact ({}), got {:?}",
                fixture["expected_no_fact_reason"].as_str().unwrap_or(""),
                evidence.map(|e| e.fact)
            );
            continue;
        }

        let evidence = evidence.unwrap_or_else(|| panic!("{id}: expected a fact, got None"));
        assert_fact_matches(id, &evidence.fact, &expected_facts[0]);
    }
}

fn assert_fact_matches(id: &str, fact: &Fact, expected: &Value) {
    let kind = expected["fact"].as_str().unwrap();
    let amt = |key: &str| expected[key].as_f64().map(Money::from_f64);
    let date = |key: &str| expected[key].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let text = |key: &str| expected[key].as_str().map(str::to_string);

    match (kind, fact) {
        ("IncomeAmountChange", Fact::IncomeAmountChange { category, amount, currency, effective }) => {
            assert_eq!(*category, expected["category"].as_str().unwrap(), "{id} category");
            assert_eq!(Some(*amount), amt("amount"), "{id} amount");
            assert_eq!(*currency, expected["currency"].as_str().unwrap(), "{id} currency");
            assert_eq!(Some(*effective), date("effective"), "{id} effective");
        }
        ("IncomeDateMoved", Fact::IncomeDateMoved { category, new_date }) => {
            assert_eq!(*category, expected["category"].as_str().unwrap(), "{id} category");
            assert_eq!(Some(*new_date), date("new_date"), "{id} new_date");
        }
        ("IncomeEnded", Fact::IncomeEnded { category, effective, description }) => {
            assert_eq!(*category, expected["category"].as_str().unwrap(), "{id} category");
            assert_eq!(Some(*effective), date("effective"), "{id} effective");
            assert_eq!(*description, text("description"), "{id} description");
        }
        ("IncomeStarts", Fact::IncomeStarts { category, amount, currency, first_date }) => {
            assert_eq!(*category, expected["category"].as_str().unwrap(), "{id} category");
            assert_eq!(Some(*amount), amt("amount"), "{id} amount");
            assert_eq!(*currency, expected["currency"].as_str().unwrap(), "{id} currency");
            assert_eq!(Some(*first_date), date("first_date"), "{id} first_date");
        }
        ("ExpenseAmountChange", Fact::ExpenseAmountChange { category, amount, percent, currency, effective }) => {
            assert_eq!(*category, expected["category"].as_str().unwrap(), "{id} category");
            assert_eq!(*amount, expected["amount"].as_f64().map(Money::from_f64), "{id} amount");
            assert_eq!(*percent, expected["percent"].as_f64(), "{id} percent");
            assert_eq!(*currency, text("currency"), "{id} currency");
            assert_eq!(*effective, expected["effective"].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()), "{id} effective");
        }
        ("OneTimeFlow", Fact::OneTimeFlow { direction, category, amount, currency, date: d }) => {
            assert_eq!(*direction, if expected["direction"] == "Credit" { Direction::Credit } else { Direction::Debit }, "{id} direction");
            assert_eq!(*category, expected["category"].as_str().unwrap(), "{id} category");
            assert_eq!(Some(*amount), amt("amount"), "{id} amount");
            assert_eq!(*currency, expected["currency"].as_str().unwrap(), "{id} currency");
            assert_eq!(Some(*d), date("date"), "{id} date");
        }
        ("Unconfirmed", Fact::Unconfirmed { category, amount, currency }) => {
            assert_eq!(*category, expected["category"].as_str().unwrap(), "{id} category");
            assert_eq!(*amount, expected["amount"].as_f64().map(Money::from_f64), "{id} amount");
            assert_eq!(*currency, text("currency"), "{id} currency");
        }
        (kind, other) => panic!("{id}: expected {kind:?}, got {other:?}"),
    }
}
