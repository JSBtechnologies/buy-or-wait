# image_transcription.v1

`prompt_version = "image_transcription.v1"`. Owner: extraction. One call per image (PLAN.md §3).
The VLM **transcribes every labeled figure on the page**. It never chooses which figure is "the"
answer — that choice is a deterministic Rust selector run after this call (PLAN.md §2.3).

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
  "amount_due_before_date": number|null,
  "amount_due_before_date_value": "YYYY-MM-DD"|null,
  "amount_due_after_date": number|null,
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
 deductions, net_pay, previous_balance, amount_due_before_date, amount_due_before_date_value,
 amount_due_after_date, document_date, period_label, line_items_sum_check}
</schema>

Rules:
- A figure you cannot read on the page is null. Never estimate, round, or infer it.
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
| expense, settled | `amount_paid` → `total` |
| expense, scheduled/pending, description implies an existing balance owed | `balance_due` → `amount_due` |
| expense, pending bill with a due-date split | if event `settlement_date` > `amount_due_before_date_value`, use `amount_due_after_date`; else `amount_due_before_date` (fallback `amount_due`) |

## Reconciliation (Rust, before the selected figure is trusted)

- `subtotal + tax == total` (tolerance 0.01)
- `gross_pay - deductions == net_pay` (tolerance 0.01)
- `amount_paid + balance_due == total`, when both present
- `line_items_sum_check` equals `subtotal` or `total`, when present
- `currency` matches the linked event's `currency`
- `document_date` is within a few days of the event's `event_date`/`settlement_date`

Any failure → escalate: re-run this same prompt once more (second read) with `temperature: 0` and
compare. Still failing, or the needed field is genuinely not present in the image (see image_04 in
`docs/gold_subset.json` for a real example: a delivery-app screenshot whose total is cropped out,
while the item subtotal reconciles exactly against the sum of its line items) → **reject the figure**.
A rejected figure is not zero and is not guessed; it is surfaced as unresolved evidence.
