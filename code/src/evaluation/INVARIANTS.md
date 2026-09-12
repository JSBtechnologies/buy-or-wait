# Evaluation: invariants, contract checks, scorer plan (verifier)

Source of truth: `problem_statement.md`, `AGENTS.md` §6, `PLAN.md` §2.10. Every rule below
marked **E** is a hard error (engine must abort, never clamp). **W** is a warning: reported,
not fatal, because the samples do not pin it down. Every E rule was checked against all 25
labelled rows in `dataset/sample_requests.csv`; all labels pass (the validator self-test runs
the labels through the same code).

## A. File-level (E)
- A1 header exactly `request_id,amount_safe_to_pay,affordability_status,recommended_payment_method,payment_plan,earliest_date_for_full_payment,spending_changes_needed,decision_explanation`
- A2 exactly one row per `request_id` in the requests file; no extras, no duplicates (W: order differs)
- A3 valid CSV, 8 fields per row

## B. Scalar fields (E)
- B1 `amount_safe_to_pay` decimal, ≤ 2 dp, `0 ≤ amount ≤ requested_amount`
- B2 `affordability_status` ∈ {affordable_now, affordable_with_plan, affordable_later, not_affordable}
- B3 `recommended_payment_method` ∈ {full_payment, partial_payment, installments, wait, not_recommended}
- B4 `earliest_date_for_full_payment` empty or `YYYY-MM-DD`, `request_date ≤ d ≤ request_date + 90d`
- B5 `amount == requested` ⇔ `earliest == request_date` (closed form §2.7: safe-today ≥ requested is the same fact)
- B6 `decision_explanation` non-empty (W: contains newline, or none of the plan's amounts/dates)

## C. Status ↔ method (E)
| status | allowed method(s) |
|---|---|
| affordable_now | full_payment |
| affordable_with_plan | partial_payment, installments, full_payment (only with spending changes) |
| affordable_later | wait |
| not_affordable | not_recommended |

## D. Payment plan (E)
- D1 `none` or `YYYY-MM-DD:amount` joined by `|`; amounts > 0, ≤ 2 dp; dates strictly increasing; first date ≥ request_date
- D2 `full_payment` ⇒ user accepts full_payment; plan = `request_date:requested_amount`
- D3 `affordable_now` ⇒ amount == requested, earliest == request_date, changes `none`
- D4 `full_payment` + `affordable_with_plan` ⇒ changes ≠ none (tuning samples 06, 11). Earliest may be > desired date.
- D5 `partial_payment` ⇒ status affordable_with_plan; `allows_partial_payment=true`; user accepts partial_payment;
  `0 < amount < requested`; exactly 2 payments: `(request_date, amount)`, `(earliest, requested − amount)`;
  earliest non-empty and ≤ desired_completion_date; sum == requested
- D6 `installments` ⇒ user accepts installments; `max_installment_months` non-blank and `number_of_payments ≤ max`;
  plan equals an `installments` option of this request exactly: n entries, date_k = first_payment_date + k·frequency_days,
  every amount == payment_amount (W: last payment > desired date — tuning samples all finish by the deadline)
- D7 `wait` ⇒ status affordable_later; user accepts full_payment; earliest non-empty and > request_date;
  plan = `earliest:requested_amount`; changes none (W: earliest > desired date — tuning samples are all ≤ desired)
- D8 `not_recommended` ⇒ plan `none`, changes `none` (W: earliest non-empty — tuning samples leave it blank)

## E. Spending changes (E)
- E1 `none` or ≤ 3 items of `stop:<event_id>` / `reduce_to:<event_id>:<amount>`
- E2 event exists, belongs to the request's user, is a debit (W: dated after request_date)
- E3 category not in `expense_categories_to_protect`
- E4 stop ⇒ flexibility ∈ {stoppable, reducible_or_stoppable} and category ∈ willing_to_stop
- E5 reduce_to ⇒ flexibility ∈ {reducible, reducible_or_stoppable}, category ∈ willing_to_reduce,
  `minimum_allowed_amount ≤ new < event amount`
- E6 no event referenced twice (covers stop + reduce on the same event)
- E7 changes ≠ none ⇒ status affordable_with_plan
- W: referenced event is not the most recent occurrence of its description+category before request_date

## F. Forecast replay (E, needs the engine's series)
- F1 plan payments applied cumulatively to the engine's day-by-day balance (with the chosen spending changes applied)
  keep `balance ≥ minimum_balance_to_keep` on every day of the horizon
- F2 (W) `amount_safe_to_pay` vs `min(requested, max(0, min_t B(t) − M))` on the no-change series
- F3 (W) suffix minimum from `earliest` ≥ requested, and from `earliest − 1` < requested

## Formatting implied by samples (W, scorer reports separately)
- `amount_safe_to_pay`: shortest decimal (`17229139.2`, `603.3`, `8401800`)
- plan / reduce_to amounts: integer when whole, else exactly 2 dp (`620.40`, `3246.10`, `23.50`, `10840`)

## Engine API (`crate::evaluation`)
- `Invariants::load(dataset_dir, requests_file)` once; per row `inv.assert_row(&model_row, &ForecastSeries{..})?` before writing;
  `inv.assert_file(&rows)?` before flushing. `Err(InvariantViolation)` is fatal. `Ok(warnings)` should be logged.
- `ForecastSeries { start: request_date, minimum, baseline: &[f64] /* closing balance per day, no request payment, no changes */,
  with_changes: Option<&[f64]> /* required when the row has spending changes */ }`
- CLI: `evaluation::cli(args)` → `validate --output F [--requests F]`, `score --output F`, `selftest`.
- Tests: `cargo test --lib evaluation`; `VERIFY_OUTPUT=<sample output csv>` scores an engine run.

## Scorer plan (sample_requests.csv)
| field | comparison |
|---|---|
| amount_safe_to_pay | numeric: exact (|Δ| ≤ 0.005), abs error, rel error, buckets ≤0.1% / ≤1% / ≤5% |
| affordability_status | exact string |
| recommended_payment_method | exact string |
| payment_plan | exact after numeric normalisation of amounts; strict-string match reported separately |
| earliest_date_for_full_payment | exact; day delta reported |
| spending_changes_needed | exact after normalisation (numeric amounts); order-insensitive match reported separately |
| decision_explanation | not exact; share of label numbers/dates present in ours |

Split: request_01–18 tuning (full diffs), request_19–25 held out (pass/fail counts only; never shared).
