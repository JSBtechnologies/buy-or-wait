# Kickoff: engine (worktree, plan mode until approved)

1. Read `fleet/AGENT_RULES.md` and `fleet/kickoffs/_plan_mode.md`.
2. Get your state (rules §5).
3. Read the plan `docs/image_accuracy_plan.md`: Context, §3, §4, Work items item 2.

## Phase B work to plan
You own `code/src/engine/`. Start with `git merge main` after the integrator's plan commit.

1. **Currency:** in `apply_fact` (`ledger.rs` ~475-488), compare the **normalized** currency (`extract::normalize::parse_currency`) with the event currency, never the raw VLM string (`Rs`, `₹`).
2. **Fail-safe:** pending or scheduled rows whose image figure failed closed must not be skipped (`forecast.rs` ~115, 130).
   - Reserve the largest amount any validated read selected.
   - Record it in `DecisionFacts.unverified_reserve` (event_id, amount, reason).
   - Agree on the candidate-amount evidence field with ml-engineer/extraction on topic `extract`.
3. **image_07:** record both totals in `DecisionFacts` (8,528 paid, 8,528.10 computed).
4. **Tests:** add scenario tests. The 104+ lib tests and `sample_report` stay green, with no tuning or held-out regression.
