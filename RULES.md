# RULES.md — reverse-engineered decision rules (analyst)

Source: `dataset/sample_requests.csv` request_01–request_18 (tuning set). request_19–25 are held out for the verifier and were NOT used to fit anything below.
Confidence tags: **[EXACT]** reproduces every tuning label that exercises it; **[FIT]** best fit, not exact on every label; **[GUESS]** not exercised by tuning labels, chosen from the spec.

Notation: `rd` = request_date, `due` = desired_completion_date, `req` = requested_amount, `M` = minimum_balance_to_keep, `B0` = current_available_balance, `methods` = payment_methods_user_will_consider (set), `safe` = amount_safe_to_pay, `E` = earliest_date_for_full_payment (Option<date>).

---

## S2. Ledger: cash rules by status, linked chains, FX [EXACT unless tagged]

`B0` (current_available_balance) already contains every **settled** row dated before `rd`. The forecast never re-applies history; history is used only to detect streams (§S3) and estimate amounts.

### S2.1 Row → forecast cash effect

| row | forecast effect | evidence |
|---|---|---|
| `settled`, settlement_date < rd | none (already in B0); feeds stream detection unless excluded below | all users |
| `pending` debit | reserve: `−amount` on `max(rd, settlement_date)` (timing irrelevant for safe; on-date and day-0 give identical labels on 01–18) | user_03 event_254 (95,000) is required to reach 873,000; user_01 event_102, user_02 event_185 |
| `pending` credit (refund, payout, bonus, commission) | **none** | spec; user_20 event_1785 (not tuned) |
| `scheduled` debit | `−amount` on settlement_date | user_04 event_357 school fee 2024-06-11 |
| `scheduled` credit (`Next confirmed salary`) | `+amount` on settlement_date; it **is** that month's salary occurrence (do not also project the stream that month) and the salary stream continues monthly afterwards at this amount | user_01 (needs 04-15/05-15 salaries to reach affordable_now), user_13, user_17 |
| `failed` | none; not a stream occurrence | user_05 event_438 |
| `cancelled` | none; not a stream occurrence | user_01 event_100, user_06 event_557 |
| `unrealized` / direction `non_cash` (investment_valuation) | none, never cash | user_21/22 (not tuned) |
| `investment_purchase` settled | historical one-off, not a stream | user_21–24 |
| `refund` settled, or any row with `linked_event_id` | historical lifecycle: exclude BOTH the row and the row it links to from stream detection (net zero, already in B0) | user_01 event_98/99, user_17 event_1543/1544 |
| authorization → settled purchase (`linked_event_id` → cancelled auth) | only the terminal settled row is real cash; still a one-off (single description) so no stream | user_01 event_100/101 |
| blank `amount` | take the figure from the linked image (§S5); never 0 | user_03 event_253, user_16 event_1442, user_17 event_1545 |
| `windfall`, bonus, commission, arrears (one-off income descriptions) | never projected | user_03 event_211, user_04 "Quarterly performance bonus", user_11 commissions, user_24 prize |

Lead DECISION verify#13 (board `decision.dup_charges`): a settled row is terminal; a pending row flagged as an open-dispute duplicate of a settled charge **stays reserved**; charge + reversal pairs are excluded from spend history.

Linked-chain rule: walk `linked_event_id` to the root; the chain is one transaction. Its cash effect = the terminal row's status per the table; every row in a chain is excluded from recurrence detection.

### S2.2 FX [EXACT on data coverage]

- Every foreign-currency row has a rate row with `rate_date == settlement_date`, `from_currency == row currency`, `to_currency == home_currency` (140/140 rows; 0 need inversion). `home_amount = amount * rate`.
- Rates are constant per pair across all dates (EUR→ZAR 20, USD→IDR 15833.33, USD→INR 83.33, EUR→USD 1.09, USD→EUR 0.92). Extra rows exist on future 15ths (e.g. 2024-04-15 USD→IDR for user_25) = **projected foreign salaries convert at the row for the projected date**. If a projected date has no row, use the latest row ≤ that date for the pair (same value).
- Do not round converted amounts (keep full precision; round only at output).

### S2.3 Same-day ordering and day boundaries [EXACT]

- Horizon: see §S3.1 (ends on the last day of month(rd)+2, not rd+90).
- Events on `rd` itself are in the forecast (user_18 utilities due 2026-07-07 = rd).
- **Within a day, debits are applied before credits.** The trough includes a debit that falls on salary day (request_18: dining on 2026-07-15 before the 2310 salary → safe 462; credits-first gives 546).
- For `E`, a payment on day d is made **after** day d's credits: `headroom_from(d) = min(end_of_day_balance(d), min_{t>d} intraday_low(t)) − M`, where `intraday_low(t)` = balance after t's debits, before t's credits. `E = first d with headroom_from(d) ≥ req`. This yields salary days (2025-09-15, 2019-11-15, 2024-06-15 …) rather than the day after.
- `safe = clamp(min_{t≥rd} intraday_low(t) − M, 0, req)`.

---

## S3. Recurrence, forecast horizon, estimators [FIT — see scoreboard S3.6]

### S3.1 Horizon [FIT, strong]

```
horizon_end = last calendar day of month(rd) + 2        # e.g. rd 2025-02-07 → 2025-04-30
days = rd ..= horizon_end
```
Not `rd+90`. Evidence (tightest bounds from tuning labels, each bound = the first projected rent that must be excluded):
- request_08 E=2025-04-15 requires the 2025-05-01 rent to be outside → end ≤ rd+82 = 2025-04-30 = eom+2.
- request_12 safe capped at 65,164 requires the 2026-07-01 rent outside → end ≤ rd+86 = 2026-06-30 = eom+2.
- request_05 outflow requires the 2026-02-02 rent outside → end ≤ 2026-02-01; eom+2 = 2026-01-31.
- request_13 E=2024-05-15 requires the 2024-06-02 rent outside; request_03 E=2019-11-15 requires end ≥ rd+74.
- Fixed H=90: E exact 13/18. eom+2: E exact 15/18 and safe within 1% on 9/18 (vs 11/18 within 5% at H=90 but only 2 exact-cap).
- The explanation text still says "90 days" (template constant).

### S3.2 Stream detection [FIT]

```
hist = rows with status == settled, settlement_date < rd, no linked_event_id, not the target of a linked row,
       event_type in {expense, subscription, debt_payment, income}, amount known (image-filled if blank)
group hist by (category, direction)
for each group:
    if direction == debit and the group has > 1 distinct description:        # variable spending (groceries, transport, dining…)
        step = modal gap in days between consecutive settlement_dates
        require every gap % step == 0                                        # gaps of 2*step = a skipped week: still the stream
        stream = Interval(step), next dates = last + k*step
    else:                                                                    # split by description
        for each description subgroup with >= 2 rows:
            if every gap in 28..31: stream = Monthly(day_of_month(last)), next = same DOM each month, clamped to month length
            else: not a stream (one-offs, bonus, arrears, commissions, irregular)
```
Worked: user_01 groceries 26 rows, 7 descriptions, gaps all 7 → Interval(7), last 2024-03-01 → 03-08, 03-15 …; user_03 rent "Landlord standing order" gaps 30/31 → Monthly(4) → 2019-09-04, 10-04, 11-04.
- Minimum occurrences: 2, 3 or 4 give identical results on 01–18 (every real stream has ≥5 rows). Use 3.
- Projected occurrences are included only if `rd <= date <= horizon_end`.

### S3.3 Amount estimators [FIT]

```
if all historical amounts in the stream are identical: amount = that value            # rent 5148, streaming 19
elif stream is Monthly  (variable bill: utilities, healthcare, shopping, entertainment): amount = mean(last 3 amounts)
elif stream is Interval (groceries, transport, dining):                               amount = mean(all amounts in history)
no rounding of estimates
```
- request_03 lead hypothesis max-of-last-3 for everything gives 872,452.60 (label 873,000) but is badly wrong elsewhere (08: −52%, 11: −36%, 13: −99%). Grid over {mean, max, median} × {last 3,4,6,8,12, all} for bills and variable spend separately; `bill=mean3, var=meanAll` maximises exact-ish matches (9/18 within 1%).
- Labels imply integer-valued outflow totals (B0 − M − safe is an integer on every EUR row: 452, 624, 1134, 487), which no history statistic reproduces. The generator most likely used hidden integer base amounts; exact recovery of those is not possible from history. Expect ±1–3% on safe amounts.

### S3.4 Income [FIT]

```
salary stream = Monthly stream in category salary (per description)
project it at its LAST settled amount on its day-of-month, unless:
  - a scheduled "Next confirmed salary" row exists → that row is the occurrence for its month; later months use its amount
  - a message amends amount / date / end (§S5 facts)               # overrides history
  - the description marks an end ("Final employer payroll")      # user_05 → no income
  - the stream missed its expected occurrence before rd            # user_12 (last 2026-01-15, rd 2026-04-05), user_13 "Second household income" (no 2024-02-20) → stop
never project: bonus, commission, arrears, prize, reimbursement, gig/platform payouts with irregular gaps (user_09, user_10)
```
Salary schedules that reproduce the tuning labels (engine test vectors):

| user | projected salary |
|---|---|
| 01 | 2024-03-15 23,320 (scheduled) then monthly 15th |
| 02 | 42,750,000 from 2025-08-15 (message_01 raise) monthly 15th |
| 03 | 4,365,000 monthly 15th (image_01 net pay confirms; arrears event_211 is one-off) |
| 04 | 38,190,000 monthly 15th; pending quarterly bonus (message_03) not counted |
| 05 | none ("Final employer payroll") |
| 06 | 1,037.52 monthly 15th (message_04 temporary pay) |
| 07 | 149,000 on 2024-09-23 (message_05 date move) then monthly 23rd → E 2024-10-23 |
| 08 | 1,422.85 monthly 15th (message_06) |
| 11 | base 23,256,000 monthly 15th; commissions not counted (message_08) |
| 12 | none (message_09 contract ended) |
| 13 | 2024-03-15 1,343.54 (scheduled) then monthly; second household income stopped |
| 14 | 2,717 from 2025-08-15 (message_10 resumes) |
| 15 | 1,661 from 2026-01-15 (message_11) |
| 16 | 173,000 monthly 15th |
| 17 | 2026-03-15 206,000 (scheduled) then monthly |
| 18 | 2,310 monthly 15th |

### S3.5 Forecast assembly

```
items = pending debits (§S2) + scheduled rows + projected streams (§S3.2–3.4), each (date, signed home amount)
per day: apply debits, record intraday_low; then credits, record end_of_day
safe  = clamp(min_t intraday_low(t) − M, 0, req)
E     = first d with min(end_of_day(d), min_{t>d} intraday_low(t)) − M >= req, else None
plan_is_safe(pays): same simulation with each payment as a debit on its date; require intraday_low − M >= 0 every day
```

### S3.6 Scoreboard for this spec (request_01–18; relative error of safe, E match)

| req | safe rel err | E | open issue |
|---|---|---|---|
| 01 | 0 (cap) | ✓ | |
| 02 | −0.7% | ✓ | |
| 03 | +11.9% | ✓ | max3 fits better here (−0.06%) |
| 04 | +8.7% | ✓ | |
| 05 | +44.8% | ✓ | no-income; outflow short |
| 06 | −18.8% | ✗ (02-15 vs 01-15) | trough composition unresolved |
| 07 | −0.8% | ✓ | |
| 08 | +0.2% | ✓ | |
| 09 | 0 (cap) | ✓ | |
| 10 | +176% | ✓ | gig income handling unresolved |
| 11 | −0.9% | ✗ (06-15 vs 07-15) | |
| 12 | 0 (cap) | ✓ | |
| 13 | +5.9% | ✓ | |
| 14 | +3.3% | ✓ | childcare payment amount unknown (message_10) |
| 15 | −100% | ✓ | trough composition unresolved |
| 16 | 0 (cap) | ✓ | |
| 17 | −0.3% | ✗ (04-15 vs 03-15) | |
| 18 | −1.6% | ✓ | |

---

## S4. request_text [EXACT, all 275 requests checked — inputs only]

- Every currency amount in `request_text` equals `requested_amount` (request_43 uses Indonesian `43.339.000` separators — same value). Every date in the text equals `desired_completion_date`; 137 texts have no date. No text contains instruction-like content.
- Phrases like "split the payment", "use installments", "pay now or wait" are template filler and do **not** correlate with `allows_partial_payment` or the user's methods (request_140: "split the payment" with allows_partial=false, methods=installments).
- **Conclusion: request_text carries no information the columns lack. Batch mode sends it to no model (0 tokens).**

## S5. Facts the engine needs from extraction

Only facts that change a forecast item. Everything else in a message is ignored. Each fact below is exercised by a tuning label (§S3.4 table shows the resulting schedule).

| fact type | fields | tuning example → engine effect |
|---|---|---|
| SalaryAmountChange | new_amount, effective_date | message_01 (user_02) 42,750,000 from 2025-08-15 → projected salary from that date; message_04 (user_06) temporary 1,037.52 "continues for the next payroll"; message_06 (user_08) next salary 1,422.85 |
| SalaryDateChange | new_date (then monthly on that day) | message_05 (user_07) 2024-09-23 → salary on 23rd, E 2024-10-23 |
| IncomeEnded | stream/employer, (date) | message_09 (user_12) seasonal contract ended → no income |
| SalaryResumes / FirstSalary | amount, date | message_10 (user_14) 2,717 from 2025-08-15; message_11 (user_15) 1,661 on 2026-01-15 |
| UnconfirmedIncome (bonus, commission, payout pending) | kind | message_03 (user_04) quarterly bonus pending; message_08 (user_11) commissions not approved (base 23,256,000 stays the projected salary — see note); message_07 (user_10) gig payout pending → **never count** |
| RecurringExpenseChange | category/stream, percent or amount, effective (next occurrence) | message_12 (user_16) rent +12% from next payment: 57,100 → 63,952 on 2023-09-01 |
| NewRecurringExpense | category, amount (required), first_date | message_10 (user_14) childcare "begins in the same month" **with no amount → cannot be counted** (do not invent) |
| OwnAccountTransfer | the matching debit/credit pair | message_13 (user_18): pair is excluded from spend history; no forecast effect in tuning |
| PayslipComposition | regular vs one-time | message_02 (user_03): payslip shows regular pay and one-time adjustment separately → the one-time part is not a salary stream |

Note on message_08 (user_11): "confirmed base salary IDR 38,760,000" conflicts with five settled 23,256,000 base-salary rows. Using 23,256,000 gives safe within 0.9% of the label; 38,760,000 only changes E-side numbers after 2025-05-15. Keep settled history as the amount unless the message states an effective date (conflict rule 3 "settled over estimate"). [FIT]

Images (blank-amount events). Selector by linked event:
| image | event | figure to use | effect |
|---|---|---|---|
| image_01 | user_03 event_253 "August 2019 net salary" (settled, history) | Net Pay 4,365,000 (equals the regular salary; not Total Earnings 4,780,800) | confirms salary stream amount; B0 already includes it |
| image_02 | user_16 event_1442 "Outstanding rent balance" (scheduled 2023-08-16) | **Balance Due 100,000** (not Total 2,00,000, not Amount Received 1,00,000) | −100,000 on 2023-08-16; request_16 stays affordable_now |
| image_03 | user_17 event_1545 "Bulk groceries and pantry purchase" (settled, history) | Cash Paid 41,272 | history only; **exclude this one-off bulk row from the groceries estimator** (including it moves request_17 safe from −0.3% to −1.3%) |

Indian digit grouping (`2,00,000.00` = 200,000) must be parsed correctly.

---

## S1. Plan candidates, eligibility, selection, status/method mapping

### S1.1 Candidate generation [EXACT on 01–18]

```
safe, E = forecast(user, changes=[])            # §S3; E may be None, may be > due
cands = []

# (a) full payment today
if 'full_payment' in methods and safe >= req:
    cands += Plan(method=full_payment, pays=[(rd, req)], status=affordable_now)

# (b) installments — one candidate per supplied option
for opt in options(request) where opt.payment_method == 'installments':
    require 'installments' in methods
    require max_installment_months != '' and opt.number_of_payments <= int(max_installment_months)
    pays = [(opt.first_payment_date + k*opt.payment_frequency_days, opt.payment_amount) for k in 0..n-1]
    require pays[-1].date <= due
    require plan_is_safe(pays)                   # §S3.6 — every day of horizon, balance - cumulative pays >= M
    cands += Plan(installments, pays, status=affordable_with_plan, total=opt.total_payable_amount, option_id)

# (c) partial payment (two payments, not an option row)
if allows_partial_payment and 'partial_payment' in methods and 0 < safe < req and E and E <= due:
    pays = [(rd, safe), (E, req - safe)]
    require plan_is_safe(pays)
    cands += Plan(partial_payment, pays, status=affordable_with_plan)

# (d) wait
if 'full_payment' in methods and E and rd < E <= due:
    cands += Plan(wait, pays=[(E, req)], status=affordable_later)
```

Worked checks:
- request_03: maxm=2, options have 21 and 24 payments → no installments; allows_partial=false; full in methods, safe 873,000 < 5,491,000, E=2019-11-15 = due → **wait**, plan `2019-11-15:5491000`, affordable_later.
- request_12: safe = req = 65,164 and E = rd, but `full_payment` ∉ methods; partial needs safe < req → no; option_33 (3 payments ≤ maxm 11, last 2026-06-20 ≤ due 2026-06-20) → **installments**, affordable_with_plan. (Shows `E` is reported independently of preferences.)
- request_02: methods = partial|installments, allows_partial=false → only option_05 (3 ≤ 7; option_07 has 18) → installments `2025-08-08:15952906.67|2025-09-07:15952906.67|2025-10-07:15952906.67`.
- request_17: maxm 3, option_47 has 3 payments → installments; option_49 (18) rejected.
- request_05: maxm 4, options 18 and 24 payments rejected; safe 737 < req, E None → nothing → not_recommended.
- request_10: methods partial|installments, option_28 has 15 > maxm 6; partial needs E → none → not_recommended.
- Installment date arithmetic: `first + k*freq_days` (request_07: 2024-09-12, +28 → 10-10, +28 → 11-07). [EXACT]
- `max_installment_months` test: `number_of_payments <= max` fits all tuning rows. The alternative `ceil(n*freq/30) <= max` gives identical results on 01–18 — both are acceptable; engine should use `n <= max`. [FIT]

### S1.2 Spending-change candidates (only when no candidate above completes by `due`) [EXACT on 06, 11]

```
if no cand in cands:                           # nothing safe without changes
    flex = recurring expense streams (§S2) whose
           category ∉ expense_categories_to_protect and
           ( (flexibility in {stoppable, reducible_or_stoppable} and category ∈ willing_to_stop)  → action stop
           | (flexibility in {reducible, reducible_or_stoppable} and category ∈ willing_to_reduce) → action reduce_to:minimum_allowed_amount )
    for k in 1..3, for each combination of k actions on distinct events (no stop+reduce on one event):
        re-run forecast with the stream's future occurrences removed (stop) or set to minimum_allowed_amount (reduce)
        re-run ONLY (a) full-now and (b) installments with that forecast      # lead DECISION rules#24: partial/wait pay on the
                                                                              # no-change E, so they cannot use a changed forecast
    pick by ranking S1.3 (change_preference = fewest changes first, lead rules#37)
```
- The event id written is the **latest settled occurrence of the stream** (request_06: streaming stream events 444,452,460,468,476 → `stop:event_476`; request_11: dining stream last row event_989 → `reduce_to:event_989:665950`).
- `reduce_to` amount = the stream's `minimum_allowed_amount` exactly (665950). [EXACT]
- request_06: user_06 willing_to_stop=streaming, gap req−safe = 620.40−603.30 = 17.10 ≤ 19 (one occurrence on 2026-01-10 inside the pre-salary trough) → stop → full payment today becomes safe → `affordable_with_plan`, `full_payment`, plan `2026-01-03:620.40`. `E` stays the no-change value 2026-01-15 (> due, so wait was not allowed).
- request_11: gap 13,110,000 − 12,510,645 = 599,355. Candidates: stop cloud_storage (168,150/occ, too small alone), reduce entertainment (next occurrence 2025-05-16 is after the 05-15 salary, so it does not lift the 05-14 trough), reduce dining (next occ 2025-05-14 inside the trough; saves ≈ estimate − 665,950 ≥ 599,355) → single change `reduce_to:event_989:665950` wins. Shows: a change only counts if it removes outflow **before the binding trough**; prefer 1 change over 2.
- Change plans still report `safe` and `E` **without** changes (spec: "before optional spending changes").

### S1.3 Ranking (spec order; consistent with all tuning rows) [EXACT where exercised]

Sort candidates by key, lowest first:
1. completes by `due` (last payment date ≤ due) — required anyway
2. number of spending changes (0 first; then 1, 2, 3)
3. total paid (installments = `total_payable_amount`; full/partial/wait = req)
4. first payment date (earlier first)
5. number of payments (fewer first)
6. payment_option_id (lowest; non-option plans before option plans on a full tie)

Consequences: full-now beats everything; partial (starts today, total=req) beats wait; wait beats installments whenever wait is eligible (lower total).

### S1.4 Status / method / plan fields [EXACT]

| winner | affordability_status | recommended_payment_method | payment_plan | spending_changes_needed |
|---|---|---|---|---|
| full now, no changes | affordable_now | full_payment | `rd:req` | none |
| full now with changes | affordable_with_plan | full_payment | `rd:req` | actions |
| installments | affordable_with_plan | installments | option schedule | none (or actions) |
| partial | affordable_with_plan | partial_payment | `rd:safe|E:req-safe` | none |
| wait | affordable_later | wait | `E:req` | none |
| nothing | not_affordable | not_recommended | `none` | none |

- `earliest_date_for_full_payment`: always the no-change `E` (blank if None), **including** not_affordable rows and rows where `E > due` (request_06 `2026-01-15` > due `2026-01-14`; request_11 `2025-07-15` > due `2025-06-12`). For affordable_now it equals rd.
- `amount_safe_to_pay`: always the no-change `safe` (capped at req), on every row including not_recommended.

### S1.5 Number and date formatting [EXACT]

Rounding rule (lead priority 3): there is **no** magnitude rounding (no floor to 100/1000). Round-looking labels (873,000; 8,401,800; 462) are round because the label generator's outflow totals are round (§S3.3), not because of an output rounding step — e.g. 17,229,139.2 and 284.57 keep B0's cents. Compute in full precision (or integer cents), round half-up to 2 dp only when writing. Partial second payment = `round2(req − safe)`; installment amounts are copied verbatim from the option row.

- `amount_safe_to_pay` column: shortest decimal repr of the value rounded to 2 dp, no trailing zeros: `17229139.2`, `603.3`, `873000`, `284.57`.
- `payment_plan` amounts and `reduce_to` amounts: integer values without decimals (`25256`, `665950`), otherwise exactly 2 dp (`620.40`, `996.60`, `15952906.67`).
- Text amounts in explanations: `CUR` + space + thousands-separated number, integers without decimals (`ZAR 25,256`, `IDR 29,158,400`), non-integers with 2 dp (`EUR 620.40`, `IDR 15,952,906.67`).
- Text dates: `D Month YYYY`, no leading zero, English month name (`8 August 2025`, `1 March 2026`).

### S1.6 decision_explanation templates [EXACT text on 01–18; variant choice partly FIT]

`{min}` = formatted M, `{req}` = formatted req, `{CUR}` = home_currency.

| case | template |
|---|---|
| affordable_now | `Pay {CUR} {req} today. This leaves at least {CUR} {min} available over the next 90 days.` (01, 16) — variant seen once (09): `Pay {CUR} {req} today. This keeps the {CUR} {min} minimum available over the next 90 days.` Use the first. |
| installments | `Use {n} installments of {CUR} {payment_amount}, starting {first_date_text}. This leaves at least {CUR} {min} available.` (02, 07, 12, 17) |
| wait, E == due | `Pay {CUR} {req} in full on {E_text}. Paying earlier would take the balance below the {CUR} {min} minimum.` (03, 08, 13, 18) |
| wait, E < due | `Wait until {E_text}, then pay {CUR} {req} in full. Paying sooner would put the {CUR} {min} minimum at risk.` (04) |
| full + stop | `Stop the {desc_lower}, then pay {CUR} {req} today. This leaves at least {CUR} {min} available.` (06: "family streaming plan") |
| full + reduce | `Reduce the {desc_lower} to {CUR} {new_amount}, then pay {CUR} {req} today. This leaves at least {CUR} {min} available.` (11: "weekend food delivery") |
| multiple changes | join clauses with `, ` and final ` and `, lowercase first letter after the first: `Stop the X and reduce the Y to {CUR} n, then pay …` [GUESS from spec example shape] |
| partial | `Pay {CUR} {safe} today and the remaining {CUR} {req-safe} on {E_text}. This completes the full request and keeps the {CUR} {min} minimum protected.` [GUESS — no tuning row] |
| not_recommended (A) | `Do not make this payment by {due_text}. None of the available options keeps the {CUR} {min} minimum protected.` (05, 10, 15) |
| not_recommended (B) | `Do not proceed with the {CUR} {req} request. Although {CUR} {safe} is available today, the full amount cannot be completed safely within 90 days.` (14) |

- `{desc_lower}` = the stream's `description` with only the first character lower-cased.
- A vs B [FIT]: B only on request_14 (methods = `partial_payment` only, allows_partial=true, safe>0, E=None). request_10 (methods partial|installments, allows_partial=true, safe>0, E=None) uses A. Rule that fits 01–18: **B iff methods == {partial_payment} and allows_partial and safe > 0 and E is None; else A.**
- The `min` in "leaves at least …" is always M itself (not the trough), so explanations need no extra forecast numbers.

### S1.7 Verifier sample surprises (rules#5), checked on 01–18 only

1. "leaves at least X" quotes `minimum_balance_to_keep`, not the trough — **confirmed** (01 ZAR 18,000; 02 IDR 29,158,400; 06 EUR 800; 07 INR 93,000 …).
2. plan / reduce_to amounts are 2 dp when fractional (`620.40`), `amount_safe_to_pay` shortest form (`603.3`) — **confirmed** (§S1.5).
3. wait dates == due: **4 of 5** tuning wait rows (03, 08, 13, 18; request_04 waits to 2024-06-15 < due 2024-06-19). Earliest dates on the 15th: 02, 03, 04, 06, 08, 11, 13, 17, 18 — **confirmed**; this is just salary day (credits land before a same-day payment, §S2.3), not a rule. Engine must not special-case due or the 15th; request_07 E = 2024-10-23 (salary moved to the 23rd).
4. full_payment + changes ⇒ affordable_with_plan even when E > due — **confirmed** (06: E 01-15 > due 01-14; 11: E 07-15 > due 06-12).
5. req_11 reduce saving needs ≥ 2 occurrences before trough — **refuted in label terms**: label gap = 13,110,000 − 12,510,645 = 599,355; the one dining occurrence before the 2025-05-14 trough saves (estimate − 665,950) ≈ 1,350,023 − 665,950 = 684,073 ≥ 599,355 with the S3.3 estimator. With the engine's own (slightly lower) safe 12,397,500 the single saving falls 28k short — so the verifier will see (5) whenever the engine's safe estimate is below the label. The rule stays: count only occurrences on or before each binding trough.
