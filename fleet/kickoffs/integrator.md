# Kickoff: integrator (main tree, plan mode until approved)

1. Read `fleet/AGENT_RULES.md` and `fleet/kickoffs/_plan_mode.md`.
2. Get your state (rules §5).
3. Read the plan `docs/image_accuracy_plan.md`: Context, §1, Work items item 1.

## Phase B work to plan
You own `code/Cargo.toml`, `src/main.rs`, `src/lib.rs`, `src/model.rs`, `src/store/`, `code/README.md`, merges into main, `output.csv`, and `code.zip`.

1. **Finish the in-progress merge** of engine `83b01c6` (E2 default on). main has `MERGE_HEAD` with engine files staged.
   - `git restore output.csv` (it's generated).
   - `cargo build` and `cargo test` through context-mode.
   - Commit the merge.
2. **Separate commit:** `docs/image_accuracy_plan.md`, `atrium.fleet.json`, `fleet/`, `PLAN.md`, `.gitignore`. Publish on topic `integrate`.
3. **Merge the branches** one at a time (`merge --no-commit --no-ff`, then build and test, then commit), in the order the lead gives: ml-engineer's OCR branch, extraction, engine.
   - The `main.rs` call fix arrives from extraction (`3df9dbf`, `cedf990`, `f70a727`). Review those hunks yourself, since `main.rs` is your file.
   - The Anthropic client must be `None` when no key is set.
4. **Load prompt v3.** Add `[selected]` to `code/config/models.toml`: `vlm_primary = Qwen/Qwen3-VL-235B-A22B-Instruct`, `vlm_escalation = google/gemma-4-31B-it`, `llm_primary` per the bakeoff, no Claude.
5. **Final:** two `--cold` runs with byte-identical `output.csv`, 250 rows; the usage report from the cold run; README; `code.zip` (exclude `target/`). Never commit `log.txt`.
