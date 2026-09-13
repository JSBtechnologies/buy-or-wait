# image_transcription.v2

`prompt_version = "image_transcription.v2"`. Owner: extraction. One call per image (PLAN.md §3).
The VLM **transcribes every labeled figure on the page**. It never chooses which figure is "the"
answer — that choice is a deterministic Rust selector run after this call (PLAN.md §2.3).

Root-cause fix (board `finding.image05_root_cause`): v1's due-date trio had an inverted name/type
pairing — `amount_due_before_date` was numeric while `amount_due_before_date_value` carried the
date string, and `amount_due_after_date` reads as a date name but is numeric. Cached reads showed
this was the actual cause of image_05's disagreement, not model vision: Qwen-235B/30B and Kimi-K3
all transcribed the correct after-cutoff figure but in inconsistent slots, while gemma-4-31B-it
followed the field names literally, wrote the cutoff date into both `*_date`-named fields, and had
no slot left for the amount. v2 replaces the trio with one unambiguous cutoff-date field and two
amount fields whose names say exactly what they hold.

## Figure schema (strict JSON, single object, no array)

```
{
  "doc_type": enum,        payslip | receipt | invoice | bill | delivery_summary |
                            bank_statement_excerpt | other
  "currency": string|null, ISO-ish code as printed (e.g. "IDR", "INR")
  "subtotal": number|null,
  "tax": number|null,
  "total": number|null,
  "amount_due": number|null,
  "amount_paid": number|null,
  "balance_due": number|null,
  "gross_pay": number|null,
  "deductions": number|null,
  "net_pay": number|null,
  "previous_balance": number|null,
  "due_cutoff_date": "YYYY-MM-DD"|null,   the date printed as the cutoff/deadline, if any
  "amount_due_by_cutoff": number|null,    amount owed if paid ON OR BEFORE due_cutoff_date
  "amount_due_after_cutoff": number|null, amount owed if paid AFTER due_cutoff_date (e.g. a late fee)
  "document_date": "YYYY-MM-DD"|null,
  "period_label": string|null,        free text as printed, e.g. "Aug-2019"
  "line_items_sum_check": number|null the model's own sum of every visible line item it transcribed
}
```

Leave a field `null` if that figure is not printed anywhere on the page. **Never estimate, round, or
infer a figure that is not visible. A field you cannot read stays null — it is not zero and it is not
a guess.** If part of the document is cut off (e.g. a receipt total below the visible crop), still
report every figure you CAN read and leave the rest null; do not fabricate the missing total.

## System prompt (stable prefix)

```
You transcribe every labeled numeric figure on a financial document image (payslip, receipt,
invoice, bill, delivery summary, or bank statement excerpt) into strict JSON. You do not decide
which figure matters — you report everything visible, verbatim. Output ONLY the JSON object below,
no prose, no markdown fences.

<schema>
{doc_type, currency, subtotal, tax, total, amount_due, amount_paid, balance_due, gross_pay,
 deductions, net_pay, previous_balance, due_cutoff_date, amount_due_by_cutoff,
 amount_due_after_cutoff, document_date, period_label, line_items_sum_check}
</schema>

Rules:
- A figure you cannot read on the page is null. Never estimate, round, or infer it.
- due_cutoff_date is the ONLY field that holds a date string. amount_due_by_cutoff and
  amount_due_after_cutoff are ONLY numbers — never write a date into either of them, and never
  write a number into due_cutoff_date.
- line_items_sum_check is YOUR sum of every individual line-item amount you transcribed (not a
  figure printed on the page) — used downstream to check your own transcription arithmetically.
- Dates as YYYY-MM-DD.
- Numbers as plain JSON numbers (no currency symbols, no thousands separators).
```

## User prompt template

```
Transcribe every labeled figure on this document. Respond with the JSON object only.
```

(Image attached as `data:image/png;base64,...` per PLAN.md §2.3/§3.)

## Deterministic selector (Rust, NOT the model) — reference for engine/extraction alignment

Given the linked event's `event_type` / `category` / `status`:

| Event kind | Selected field (first non-null wins) |
|---|---|
| income, settled/scheduled | `net_pay` → `total` |
| expense, settled | `total` → `amount_paid` |
| expense, scheduled/pending, description implies an existing balance owed | `balance_due` → `amount_due` |
| expense, pending bill with a due-date split | if event `settlement_date` > `due_cutoff_date`, use `amount_due_after_cutoff`; else `amount_due_by_cutoff` (never falls back to the other side, or to `balance_due`/`amount_due`, once a cutoff is known) |

## Reconciliation (Rust, before the selected figure is trusted)

- `subtotal + tax == total` (tolerance 0.5 per printed term, 1.0 combined)
- `gross_pay - deductions == net_pay` (same tolerance)
- `amount_paid + balance_due == total`, when both present (or `amount_paid >= total` with
  `balance_due` absent/~0 and `amount_paid < 2x total`, for a cash-tendered-with-change receipt)
- `line_items_sum_check` equals `subtotal` or `total`, checked only when no labeled-field
  identity above had the data to run at all
- `currency` matches the linked event's `currency`

Any failure → escalate: re-run with a second, independent reader and compare (`decision.vlm_setup`
agreement mode) or re-run this same prompt once more (escalate mode). Still failing, or the needed
field is genuinely not present in the image (see image_04 in `docs/gold_subset.json` for a real
example: a delivery-app screenshot whose total is cropped out, while the item subtotal reconciles
exactly against the sum of its line items) → **reject the figure**. A rejected figure is not zero
and is not guessed; it is surfaced as unresolved evidence.
