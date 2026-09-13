# Plan: Normalization-first, verified-or-fail-closed image preprocessing

## Context
- 16 events have blank amounts whose values are printed on images: real-world receipts, bills, a payslip, and a handwritten pharmacy bill.
- The user's bar is **verified-or-fail-closed**: wrong figures = 0, nothing unproven is guessed, and every cold run gives the same result. Model testing hasn't met that bar, and it's blocking the finish (about 10h left, deadline 2026-09-13T12:30Z).
- **The user's diagnosis, which the evidence supports: most failures are normalization, not reading.**
  - image_02: Indian lakh grouping read 10× too large.
  - image_12: `$33,50` comma decimal, on a USD event.
  - image_05: cutoff-field confusion, fixed by schema v2.
  - image_07: rounding, 8,528.10 vs 8,528.
  - image_11: a detailed breakup that doesn't sum to the final amount (likely cut off). It must not override the printed final total.
- **The plumbing is also broken.**
  - `[selected]` is absent, so production makes 0 model calls.
  - `main.rs:262-271` passes 8 args where `resolve_blank_amount` takes 10 (`images.rs:756-767`).
  - `main.rs:123` loads prompt v1.
  - `ledger.rs:475-488` compares the raw VLM currency (`Rs` / `₹`) with `INR` and rejects valid figures.
- **What matters most.** Only 4 images change a forecast: 02 (scheduled), 05 (pending), 10 (pending) and 11 (scheduled). The other 12 are settled rows already inside the balance.
- **Claude.** The tiebreak is capped by the Anthropic org limit until 2026-10-01, so the default chain is HF-only. Claude stays optional if a key or limit is available.

## Approach

### 1. The model transcribes raw strings; Rust normalizes (the core fix)
Prompt v3 (`code/prompts/image_transcription.v3.md`, extraction):
- The model copies every candidate figure **verbatim as printed**: `"2,00,000.00"`, `"$33,50"`, `"41272.0"`, `"9,124.0\n0"`, `"06-Feb-2026"`.
- It also copies the witnesses: `amount_in_words`, `line_items[]`, `charges_breakdown[]`, `amount_paid`, `balance_due`, `grand_total`.
- It does no number conversion and never decides which figure matters.

Rust normalization (`code/src/extract/normalize.rs`, extending `parse_amount` `:33`, `parse_currency` `:246`, `parse_date` `:293`):

| Trap | Rule | Image |
|---|---|---|
| Indian lakh/crore grouping | `d,dd,ddd(.dd)` groups are valid when the currency is INR → `2,00,000.00` = 200000 | 02, 10 |
| Comma decimal | Currency USD/EUR with a trailing `,dd` and no other separator → decimal comma → `$33,50` = 33.50 | 12 |
| Trailing `.0` / wrapped cells | Strip a line break inside a number cell, then parse → `9,124.0\n0` = 9124.00 | 03, 15 |
| Currency symbols | `₹`, `Rs`, `Rs.`, `INR`, `Rupees` → INR; `$` → USD; `Rp`, `IDR`, `Rupiahs` → IDR. **Normalize before `apply_fact`.** | all |
| Dates | `DD-Mon-YYYY`; `DD/MM/YY` and `DD/MM/YYYY` resolved against the event date (the closest valid reading within the window wins; ambiguity that can't be resolved means no date) | 02 `11/08/23`, 03, 05, 12 `01/10/2025` |
| Amount in words | English words to number, including lakh/crore, paise/cents, "Rupiahs" | 01, 05, 06, 08, 09, 10, 16 |

### 2. A witness check confirms the normalized figure
A figure F is **accepted** only when all of these hold:
- 2 reads select the same normalized F, and agree on the cutoff if one exists;
- at least one witness proves F: line-item sum, subtotal + taxes, gross − deductions, amount in words, total − paid = balance, or the same final amount repeated under another final label (e.g. Total Bill = Amount Payable = Balance);
- no **final-labeled** figure contradicts F.

**Final amount is truth (user rule).** A printed final amount (Total, Grand Total, Total paid, Amount Payable, Balance / Balance Due, Net Pay, Cash Paid) is authoritative. Detailed breakdowns and sub-tables are supporting evidence only:
- if a breakdown sums to F, it counts as a witness;
- if it doesn't, it is **not** a contradiction and never rejects F, because pages are often cut off. The mismatch is logged as a read note only, never surfaced in the explanation.

Otherwise the image fails closed. The paid≥total shortcut in `reconciles` (`images.rs:393-455`, the cause of FA1) is removed. `select` (`:295-374`) and `validate_doc` (`:552-585`) are kept.

**Read budget (HF-only):** Qwen3-VL-235B@1024 and gemma-4-31B@768. If the gate isn't met, add up to 2 more reads (235B@1536, gemma@1024) and stop as soon as it passes.

### 3. Per-image rulings (user decisions captured)
- **image_02 (rent balance, scheduled):** 100,000 balance due. Witness: 2,00,000 total − 1,00,000 received.
- **image_05 (telecom, pending) — FLEET WAS CORRECT (user confirmed):** **822.05**, the amount due after the 06-Feb-2026 cutoff. The event is an *Outstanding* telecom bill (unpaid), `status=pending`, `settlement_date=2026-02-09` (dataset/financial_events.csv event_1786), so the payment lands after the due date and the late amount applies. Keep the current gold (`hardcode_scan.rs:86`), the trap test (`image_agreement.rs:499-501`), and the `select` cutoff branch (`images.rs:295-374`) unchanged.
  - Witnesses: pre-cutoff 704.05 is proven by 580.65 + 16.00 + 107.40 and by the words. 822.05 is accepted when 2 reads agree on it and on the cutoff, and it is greater than the proven 704.05 (late fee 118 > 0).
- **image_07 (restaurant, settled) — USER CONFIRMED:** **8,528**, the Grand Total actually paid. Witness: round(8,122 + 203.05 + 203.05) = round(8,528.10). Log both values in `DecisionFacts`.
- **image_11 (hospital, scheduled, amount paid 0) — USER CONFIRMED: 3,650.**
  - The final amount is truth: Total Bill Amount = Amount Payable = Balance = 3,650, and the user pays the bill.
  - Witnesses: summary lines 1,650 + 1,000 + 1,000 = 3,650; the three final labels repeat it.
  - The detailed breakup (Professional Fees section shows 500 and no Subtotal row, unlike every other section) is treated as likely cut off. It doesn't reject 3,650 and isn't mentioned in the explanation.
  - Event context: event_6859 "Hospital bill payable", `scheduled`, bill date 2023-01-19, settles 2023-01-23; no messages or healthcare history. request_73 is a 71,400 repair whose safe amount is likely sensitive to this figure.
- **image_12 (taxi, USD):** 33.50 USD. Witness: 28.50 + 5.00, tax 0. Cash 40.00 and change 6.50 are ignored. Convert to INR with the `exchange_rates.csv` row for settlement date 2025-10-01.
- **image_04:** the total is cropped, so it fails closed. It's a settled, history-only row, so this is safe.

### 4. Fail-safe for forecast-affecting rows
Pending or scheduled rows (02, 05, 10, 11) that fail closed must never be silently skipped. The fix goes in `forecast.rs:115,130`, which today drops `Missing` rows. Instead, reserve the largest amount any validated read selected, and record it in `DecisionFacts` as `unverified_reserve`.

## Work items → owners
1. **integrator:** fix the `main.rs` call to `resolve_blank_amount` (10 args); load prompt v3; add `[selected]` to `code/config/models.toml`; clear the stray uncommitted diffs in the extraction and ml-engineer worktrees.
2. **engine:** normalize currency in `apply_fact` (`ledger.rs:475-488`); add `unverified_reserve` to `DecisionFacts`.
3. **extraction:** prompt v3 (raw strings + witnesses); the `normalize.rs` rules above; a new `witness.rs` (words parser, sums, identities); replace the acceptance logic (`images.rs:1054-1109`) with the witness gate.
4. **ml-engineer:** live N=5 cold harness in `bakeoff.rs`, with every raw read persisted per run (no cache replay); per image × run table.
5. **verifier:** align names with extraction (`image_agreement.rs` roles, outcomes, classes); checks: witness named for every accept; 0 false accepts vs gold and analyst reads; accepted figure identical across the 5 runs; IA9 clear.
6. **analyst:** RULES.md S5 per-image rulings and witness spec (as in §3).

**Order:** 1 + 2 (unblock a real run) → 3 → 4 → 5. Item 6 runs alongside. After approval, append this turn's entry to `log.txt`; logging was not possible in plan mode.

## Verification
1. `cargo test` on normalization and witness unit tests, using hand-transcribed strings from the 16 images as dev-only fixtures (including a test that a non-summing breakdown never rejects a corroborated final amount):
   - `2,00,000.00` → 200000
   - `$33,50` → 33.50
   - `9,124.0\n0` → 9124
   - words → number for 01, 05, 06, 08, 09, 10, 16
   - `11/08/23` → 2023-08-11
2. Live N=5 cold bake-off on all 16 images:
   - accepted figure identical in 5/5 runs, or fail-closed identically;
   - **0 wrong figures**;
   - 02, 05, 10 and 11 accepted in 5/5 runs (image_11 at 3,650 despite the breakup mismatch).
3. The verifier signs off with no naming drift.
4. `cargo run --release -- --cold` twice: the two `output.csv` files are byte-identical, and `usage_report.md` calls match the read budget.
5. Sample scorer: no regression on tuning or held-out, and request_16 (image_02) and request_20 (image_05) unchanged or improved.

## Execution phases (added at handoff)
- **Phase A (OCR first):** only **lead** and **ml-engineer** execute. ml-engineer temporarily owns the image files (`extract/images.rs`, `extract/normalize.rs`, new `extract/witness.rs`, `prompts/image_transcription.v3.md`, `bin/bakeoff.rs`). It merges the extraction branch first, then implements sections 1–3 and runs the live N=5 persisted-read test (work item 4).
- **Phase A DONE criteria:** 0 wrong figures; accepted figures identical across 5/5 runs; images 02/05/10/11 accepted 5/5; image_04 fails closed. The lead posts `PHASE A DONE <sha>`.
- **Everyone else starts in plan mode** and writes an executable Phase B plan. The user approves each plan after Phase A DONE. Log entries are kept as a backlog and appended right after approval.
- **Phase B:** integrator (finish the 83b01c6 merge, commit plan/config, merge the OCR branch, `[selected]`) and engine (item 2) run in parallel, then extraction absorbs the OCR commits and takes ownership back, then verifier (item 5), then the final cold runs and ship.
