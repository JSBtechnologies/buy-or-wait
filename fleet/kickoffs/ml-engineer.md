# Kickoff: ml-engineer (worktree, automode): Phase A executor

1. Read `fleet/AGENT_RULES.md`. Write your SESSION START entry. Get your state (rules §5).
2. Read `docs/image_accuracy_plan.md` in full.

## Your job: finish and verify OCR
You temporarily own the Phase A image files (rules §9), plus your usual files: `hf.rs`, `bin/bakeoff.rs`, `config/models.toml`, `docs/bakeoff.md`, `evaluation/usage_report.md`.

1. **Sync:** `git merge atrium/buyorwait/extraction`, which brings routing v3 and the `main.rs` call fix `3df9dbf`. Clean up any stray edits to files you don't own (rules §6).
2. **Prompt v3** (plan §1): the model copies every candidate figure **verbatim as printed**, plus the witness fields. It does no conversion and never picks the figure.
3. **normalize.rs** (plan §1 table): lakh/crore grouping, comma decimals, wrapped cells and trailing `.0`, currency symbols to ISO, dates resolved against the event date, words to number.
4. **witness.rs and the witness gate** (plan §2), replacing the agreement logic in `images.rs`:
   - accept F only if 2 reads agree on normalized F (and on the cutoff, if present) and at least 1 witness proves F;
   - no final-labeled figure may contradict F;
   - a non-summing breakdown is a note, never a rejection;
   - remove the paid≥total shortcut;
   - use the HF-only read budget, stopping at the first pass.
5. **Per-image rulings** (plan §3, rules §8), with unit tests using hand-transcribed fixtures. Start from the plan's witness map; the analyst adds verbatim strings later.
6. **No Anthropic client** anywhere on the resolution or bakeoff path (`None` when no key).
7. **Live N=5 cold run**, through the production path in `images.rs`, on all 16 images:
   - check HF credits first;
   - persist **every** raw read per run, with no cache replay;
   - write a per image × run table to `docs/bakeoff.md`: reads used, normalized figure, witness type, accept or fail-closed, tokens.
8. **Iterate with the lead** until Phase A DONE holds. Commit, then post `bakeoff msg=ml-engineer: PHASE A candidate <sha> <summary>`.

Keep `cargo test` green, and send all cargo and bakeoff output through context-mode.
