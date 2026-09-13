# Kickoff: extraction (worktree, plan mode until approved)

1. Read `fleet/AGENT_RULES.md` and `fleet/kickoffs/_plan_mode.md`.
2. Get your state (rules §5).
3. Read the plan `docs/image_accuracy_plan.md`: §1–§3, Work items item 3.

## Phase B work to plan
You own `code/src/extract/` and `code/prompts/`. During Phase A, ml-engineer temporarily owns the image files (rules §9) and merges your branch.

1. **Cleanup:** `git restore code/evaluation/usage_report.md` (ml-engineer's file). It's your only uncommitted change.
2. **Absorb ml-engineer's OCR commits:** prompt v3, `normalize.rs`, `witness.rs`, the witness gate in `images.rs`. Review them for correctness against plan §1–§3, then take ownership of those files back.
3. **Message side:** confirm message and intake extraction still passes with the shared `normalize.rs` changes (currency and date rules). Fix any regressions.
4. **Candidate field:** expose the selected-candidate amounts that engine's `unverified_reserve` needs (agree on the field name on topic `extract`).
5. **Tests:** fixtures from the analyst's verbatim strings, including:
   - `2,00,000.00` → 200000
   - `$33,50` → 33.50
   - `9,124.0` + newline + `0` → 9124
   - words → number for images 01, 05, 06, 08, 09, 10, 16
   - `11/08/23` → 2023-08-11
   - a non-summing breakdown that doesn't reject a corroborated final amount
