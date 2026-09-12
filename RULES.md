# RULES.md — reverse-engineered decision rules (analyst)

Source: `dataset/sample_requests.csv` request_01–request_18 (tuning set). request_19–25 are held out for the verifier and were NOT used to fit anything below.
Confidence tags: **[EXACT]** reproduces every tuning label that exercises it; **[FIT]** best fit, not exact on every label; **[GUESS]** not exercised by tuning labels, chosen from the spec.

Notation: `rd` = request_date, `due` = desired_completion_date, `req` = requested_amount, `M` = minimum_balance_to_keep, `B0` = current_available_balance, `methods` = payment_methods_user_will_consider (set), `safe` = amount_safe_to_pay, `E` = earliest_date_for_full_payment (Option<date>).

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
        re-run (a)(b)(c)(d) with that forecast
    pick by ranking S1.3 (fewest changes first among change plans — see below)
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
