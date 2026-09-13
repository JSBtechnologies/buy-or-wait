# Kickoff: verifier (worktree, plan mode until approved)

1. Read `fleet/AGENT_RULES.md` and `fleet/kickoffs/_plan_mode.md`.
2. Get your state (rules §5).
3. Read the plan `docs/image_accuracy_plan.md`: §2, §3, Work items item 5, Verification.

## Phase B work to plan
You own `code/src/evaluation/`. Be adversarial, don't rubber-stamp.

1. **Cleanup:** `git restore code/evaluation/usage_report.md` (ml-engineer's file).
2. **Align names:** match `image_agreement.rs` roles, outcomes and classes to what ml-engineer's OCR branch actually ships. Confirm on topic `extract`.
3. **New checks:**
   - every accepted figure names its witness;
   - 0 false accepts vs gold (`hardcode_scan.rs`, dev-only) and analyst reads;
   - accepted figure identical across the 5 persisted runs;
   - a non-summing breakdown never rejects a final amount;
   - IA9 clear.
4. **Gold:** keep image_05 = 822.05 and the 704.05 trap test. Set image_11 = 3,650 and image_07 = 8,528 if they differ.
5. **Ship:** validate the final `output.csv` contract, confirm the two cold runs are byte-identical, run the sample scorer (no regression), and post signoff on topic `integrate`.
