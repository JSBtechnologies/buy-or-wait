# Postmortem — Buy or Wait? (HackerRank Orchestrate, September 2026)

An honest look at what worked, where I fell short, and what I'd do differently. Written after results were released.

## Result

- **Rank: 141 of ~3,000 participants: top ~4.7% (95th percentile).**
- **Total: 65.2 / 100.** A strong placement, but well short of the score the design aimed for. There was real room to improve.

| Component | Score | % |
|---|---|---|
| Chat transcript (`log.txt`) | 9.1 / 10 | 91% |
| Code (`code.zip`) | 21.9 / 30 | 73% |
| Output (`output.csv` vs hidden ground truth) | 17.1 / 30 | 57% |
| AI Judge interview (hidden test cases) | 17.1 / 30 | 57% |

What the submission got right:
- **Output contract:** 0 violations across 250 rows.
- **Image figures:** 0 wrong figures accepted.
- **Determinism:** byte-identical cold and warm runs.
- **Sign-off:** a full verification gate passed before shipping.

**Correctness of form and safety of evidence were strong. Numeric accuracy and interview readiness were not.**

## What I built (short)

- **Engine:** a deterministic Rust engine rebuilds each user's ledger, forecasts cash day by day against their minimum balance, and searches and ranks payment plans. Explanations come from recorded facts.
- **Receipts:** OCR'd once at ingestion by `baidu/Unlimited-OCR`, self-hosted on vLLM on a RunPod H100.
  - Rust maps the printed labels with fixed keyword rules.
  - A figure is accepted only through a witness gate; otherwise it fails closed.
- **Development:** run solo, using an [atrium](https://github.com/nativelite/atrium) multi-agent fleet (lead, analyst, engine, extraction, ml-engineer, verifier, integrator).

## Where I failed

### 1. The output score: `amount_safe_to_pay` accuracy (the biggest loss)
- **Sample results:** decisions were mostly right. On the 7 held-out samples, status, method and earliest date were 7/7. **But the exact safe amount matched 0/7** (4/7 within 1%), and only **4/18** on the tuning samples.
- **Root cause: under-projected variable spending.**
  - Our residual diagnostic (`engine::samples::residuals::safe_residuals`) showed the largest misses were **1–5.5% errors in projected spending**:
    - request_05: 1.0% of 32,638;
    - request_10: 5.5% of 512,055;
    - request_15: 2.7% of 487.
  - The safe amount is a thin leftover (balance − minimum − spending to the low point), so those small errors became **+45%, +223% and +16%** errors in the safe amount.
- **The errors were in variable streams** (groceries, transport, dining): **how much** was projected (mean of all history vs recent weighting) and **how often** (the phase of weekly cycles). Fixed bills, rent and salary lined up.
- **I believed the engine was conservative. It wasn't, for variable spending.** Half the tuning misses were **optimistic**, the unsafe direction for a product whose whole promise is "don't tell someone they can afford it when they can't". I also said "conservative" in my own explanations before checking that.
- **Explanations lost points as a knock-on:** they quote the safe amount, so a small amount miss failed the explanation too.
- **Convention choice:** I filled `earliest_date_for_full_payment` on four `not_affordable` rows, a literal reading of the spec, while every sample left it blank. Defensible, but it likely cost points.

### 2. Too much time on image extraction relative to its scoring weight
- **Where the hours went:** most of the second half went into getting 16 receipt images to "zero wrong figures":
  - a two-model VLM setup;
  - tiebreaks, including a Claude backup blocked by an org usage cap;
  - Hugging Face credit exhaustion mid-run;
  - three prompt versions;
  - a full switch to OCR with a new serving stack.
- **Why that was too much:**
  - **Only 11 images belonged to evaluation users, and most were settled past purchases** that barely move a forecast.
  - Meanwhile the thing that decided the output score, variable-spending calibration across all 250 users, got far less attention after early tuning.
- **The final image pipeline was genuinely good.** OCR plus deterministic mapping plus a witness gate was the right idea, and it came from a real insight: the models *read* amounts correctly but *mapped* them wrong. **But I should have time-boxed it and moved on sooner.**

### 3. Architecture churn right up to the deadline
- **What changed in the last hours:** a reader swap (HF VLM → OCR), a serving swap (local transformers → vLLM/RunPod), a rules rewrite ("total trumps all"), removal of the legacy paths, and README and doc rewrites, all within a few hours of submission.
- **The cost:**
  - several rebuild, verify and re-zip cycles;
  - late risk (a broken build on main during the cleanup);
  - less time to prepare and rehearse.

### 4. Interview readiness (17.1/30)
- **Hidden test cases probed areas that were still moving or unfinished:**
  - **Ask mode:** free text → Qwen3-235B → request JSON existed in code, but was **never activated**. It **skipped message and image evidence**, so its answers could differ from batch. I connected it and proved it matches batch **after** submission, not before.
  - **Unreadable pending-bill image:** the engine supports a conservative placeholder reserve (`Fact::UnverifiedEventAmount`), but the OCR path **never emits it**. A pending or scheduled bill whose image fails would be skipped, which is optimistic. The submitted data didn't trigger it, but it's a real gap.
  - **Details I had to re-derive under pressure:** the forecast horizon (end of month + 2, fitted from samples), same-day debit/credit ordering, the model inventory.
- **My explanation mixed up facts** in early rehearsals: which run used which model, where the "buffer" comes from (conservative forecasting, not rounding), and what "isolated builds" meant. I caught these in rehearsal, but late.

### 5. Code package (21.9/30)
- **A cold run needs a self-hosted GPU OCR endpoint,** so a grader can't easily reproduce the output end to end. A warm run from a shipped OCR cache would have been easier to evaluate.
- **The codebase carried a lot of evaluation and verification tooling.** That's valuable, but it adds weight to a package reviewed for clarity.
- **Late churn** in README and structure right before zipping.

## Process issues (running a multi-agent fleet)

Most of these came from speed and fatigue, not from the design:
- **Accidental plan approvals:** plan-mode agents were approved by mistake and began executing Phase B work early. I caught it and halted them.
- **A shared `CARGO_TARGET_DIR`** across worktrees contaminated build caches (false build errors, risk of false-passing tests). Fixed by giving each worktree its own target folder, then re-verifying every earlier result.
- **The C: drive filled to 0 bytes** mid-run (download caches). Recovered by clearing the pip cache.
- **A git branch switch deleted working-tree files:** archiving ignored files on a branch, then switching back, removed them from disk, including the persisted OCR reads that the sign-off verifies. An outside sign-off run failed in that window until the files were restored.
- **Running all night without sleep** made the final hours error-prone (mis-stated facts, rushed rulings).

## What went well

- **Verification as part of the product:**
  - contract validator;
  - forecast invariants;
  - a false-accept gate on image figures;
  - a check that shipped rows equal re-derived engine rows;
  - a scan for hard-coded answers;
  - byte-identical cold runs.

  Nothing shipped without passing.
- **Generic rules instead of per-case patches,** so each bug fix covered a whole class of inputs, not just the one that exposed it.
- **The image insight:** separating transcription (model) from interpretation (Rust), and failing closed. 0 wrong figures accepted.
- **Decision quality on held-out samples:** status, method and earliest date 7/7.
- **Held-out discipline:** samples 19–25 were never used for tuning.
- **The atrium fleet made a solo 24-hour build feasible:** parallel specialists, visible panes, a shared board and bus, a single merger, and a full audit trail (`log.txt` scored 9.1/10).

## What I'd do differently

1. **Calibrate variable spending against the samples first,** before any extraction work:
   - fit the average (all history vs recent) and the weekly occurrence phase jointly across all samples;
   - measure error as a **percentage of projected spending**, not of the safe amount.
2. **Make "conservative" real, then prove it:**
   - add an explicit safety margin to variable spending (e.g. the higher of recent and overall average);
   - add a test that fails if the engine is ever more optimistic than the answer key on samples.
3. **Time-box extraction by scoring impact.** Weight effort by how many evaluation rows and how much forecast movement each piece affects.
4. **Follow sample conventions** on ambiguous fields, unless there's strong evidence the spec means otherwise.
5. **Freeze the architecture at least 4–6 hours before the deadline.** After that, only fixes, rehearsal and packaging.
6. **Close half-built features or cut them before submission.** Here that meant ask mode and the unverified-bill reserve.
7. **Ship a warm-runnable package:** include the OCR cache so graders can reproduce the output without a GPU, plus a one-command run.
8. **Prepare the interview in parallel:** a one-page fact sheet (models per run, horizon, ordering rules, gaps) written while building, not after.
9. **Sleep.** Even a short break before the final push would have prevented several late mistakes.

## Timeline (UTC, abridged)

- **12 Sep 20:26:** fleet kickoff. Scaffold, rule reverse-engineering from samples, model bake-off.
- **12 Sep 23:00 – 13 Sep 02:00:**
  - two-model VLM agreement for images, then Kimi and Claude backup attempts;
  - HF credits exhausted;
  - engine rule toggles A/D/E2 chosen by held-out A/B;
  - routing v3 image gate.
- **13 Sep 03:00 – 06:00:**
  - Phase A OCR work;
  - audit found VLM mapping (not reading) failures;
  - rulings on images 02/05/07/11/04;
  - engine Phase B done.
- **13 Sep 06:50 – 09:30:**
  - local Unlimited-OCR experiment on an RTX 3070;
  - multi-page and upscale fixes;
  - vLLM on RunPod H100 live;
  - 16/16 byte-identical.
- **13 Sep 09:45 – 11:30:**
  - Phase A done;
  - merges;
  - request_16 engine fix;
  - "total trumps all" rulings;
  - legacy HF VLM and Anthropic paths removed;
  - final sign-off.
- **13 Sep 11:30 – 12:15:** final cold run confirmation, README and architecture docs, submission.
- **13 Sep 21:00 – 23:45:** interview prep. Ask mode connected to batch evidence (post-submission).
- **Results:** 65.2/100, rank 141/~3,000.

## Bottom line

A top-5% finish built on a sound, verified architecture. But I optimized for evidence safety on a small slice of the data while the score was decided by forecasting accuracy across all of it, and I kept changing the design too close to the deadline. The fundamentals (determinism, verification, fail-closed evidence, agent orchestration) are worth keeping. The next build should calibrate the core numbers first, freeze earlier, and prepare the explanation as it goes.
