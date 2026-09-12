//! Deterministic decision engine (owner: engine). Pure, no I/O, no network, no model calls.
//!
//! Flow: `Session::open` (one user) -> `Ledger` (§2.1) -> `recurrence` streams (§2.2) ->
//! `Forecast` (§2.7) -> `plans` search + ranking (§2.8) -> `DecisionFacts` (§2.10) ->
//! `explain` templates.

pub mod money;
pub mod rules;
pub mod types;

pub mod session;
pub mod ledger;
pub mod recurrence;
pub mod forecast;
pub mod plans;
pub mod facts;
pub mod explain;


pub use rules::Rules;

mod samples;
