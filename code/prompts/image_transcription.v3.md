# image_transcription.v3

`prompt_version = "image_transcription.v3"`. Owner: ml-engineer (Phase A, image_accuracy_plan.md
§1). One call per image (PLAN.md §3). The VLM **transcribes every labeled figure on the page,
verbatim as printed** — it does no number conversion, and it never chooses which figure is "the"
answer. That choice is a deterministic Rust selector run after this call (PLAN.md §2.3), and the
conversion from printed text to a number is deterministic Rust normalization
(`extract::normalize::parse_amount`, image_accuracy_plan.md §1 table), not the model's job.

Root-cause fix (image_accuracy_plan.md §1): v1/v2 asked the model to convert each figure to a
plain JSON number itself, which is exactly where the Indian lakh-grouping 10x misread (image_02)
and the comma-decimal misread (image_12, `$33,50` read as 3350 instead of 33.50) happened — a
model's own arithmetic on an unfamiliar grouping convention is not reliable, even though its
*vision* (reading the printed characters) usually is. v3 asks for the printed string ONLY;
`extract::normalize` (analyst RULES.md S7) owns every grouping/decimal/date convention this
dataset uses, so the same misread traps get the same deterministic fix on every model and every
future image, not a per-model prompt tweak.

v3 also adds the witness fields the image_accuracy_plan.md §2 witness gate needs: every candidate
figure is trusted only once TWO independent reads agree on it AND some independent identity on
the page itself proves it (a line-item sum, a subtotal+charges breakdown, an amount spelled out
in words, or the same final amount repeated under a second label) — the model must transcribe
those witnesses too, verbatim, for the gate to have anything to check.

## Figure schema (strict JSON, single object, no array)

```
{
  "doc_type": enum,        payslip | receipt | invoice | bill | delivery_summary |
                            bank_statement_excerpt | other
  "currency": string|null, exactly as printed (e.g. "Rs", "₹", "IDR", "$") -- do not convert to
                            an ISO code yourself
  "subtotal": string|null,             copy the printed text verbatim, e.g. "3,543.54", "30.780.000"
  "tax": string|null,
  "total": string|null,
  "grand_total": string|null,          ONLY when the page prints a SEPARATE "Grand Total" line
                                        distinct from "Total" (e.g. a service charge added after
                                        a subtotal-style total) -- otherwise leave null, do not
                                        repeat "total" here
  "amount_due": string|null,
  "amount_paid": string|null,
  "balance_due": string|null,
  "gross_pay": string|null,
  "deductions": string|null,
  "net_pay": string|null,
  "previous_balance": string|null,
  "due_cutoff_date": "YYYY-MM-DD"|null,   the date printed as the cutoff/deadline, if any
  "amount_due_by_cutoff": string|null,    amount owed if paid ON OR BEFORE due_cutoff_date, verbatim
  "amount_due_after_cutoff": string|null, amount owed if paid AFTER due_cutoff_date, verbatim
  "document_date": string|null,       copy the printed date text verbatim, e.g. "11/08/23"
  "period_label": string|null,        free text as printed, e.g. "Aug-2019"
  "amount_in_words": string|null,     the printed "amount in words" line verbatim, e.g.
                                       "Rupees Seven Hundred Four and Five Paise Only"
  "line_items": [string]|null,        every individual line-item amount printed, each copied
                                       verbatim, in the order printed (e.g. ["580.65", "16.00",
                                       "107.40"]) -- omit a line item you cannot read rather than
                                       guessing it, do not sum them yourself
  "charges_breakdown": [string]|null  a SEPARATE itemized list of charges/taxes/deductions
                                       distinct from line_items (e.g. two service-charge lines
                                       added on top of a subtotal), each copied verbatim
}
```

Leave a field `null` (or `[]` for the two arrays) if that figure is not printed anywhere on the
page. **Never estimate, round, sum, or convert a number yourself — copy exactly what is printed,
including its commas, dots, currency symbol, and any line break inside the cell. Never infer a
figure that is not visible: a field you cannot read stays null, it is not zero and it is not a
guess.** If part of the document is cut off (e.g. a receipt total below the visible crop), still
report every figure you CAN read and leave the rest null; do not fabricate the missing total.

## System prompt (stable prefix)

```
You transcribe every labeled numeric figure on a financial document image (payslip, receipt,
invoice, bill, delivery summary, or bank statement excerpt) into strict JSON, copying every
number and date EXACTLY as printed -- as a string, with its original commas, dots, currency
symbol, and line breaks. You do not convert, sum, round, or compute anything, and you do not
decide which figure matters -- you report everything visible, verbatim. Output ONLY the JSON
object below, no prose, no markdown fences.

<schema>
{doc_type, currency, subtotal, tax, total, grand_total, amount_due, amount_paid, balance_due,
 gross_pay, deductions, net_pay, previous_balance, due_cutoff_date, amount_due_by_cutoff,
 amount_due_after_cutoff, document_date, period_label, amount_in_words, line_items,
 charges_breakdown}
</schema>

Rules:
- A figure you cannot read on the page is null (or [] for line_items/charges_breakdown). Never
  estimate, round, sum, or infer it.
- Every amount and date field is a STRING holding exactly what is printed -- never convert a
  grouped number ("2,00,000.00", "30.780.000") or a comma-decimal ("$33,50") to a different form,
  and never reformat a date. If a number wraps across a line break inside its cell, copy it with
  the break as printed (e.g. "9,124.0\n0").
- due_cutoff_date is the ONLY field that holds a normalized "YYYY-MM-DD" date; every other date
  field (document_date) is copied verbatim in whatever format is printed.
- amount_due_by_cutoff and amount_due_after_cutoff are printed AMOUNTS, never a date.
- grand_total is ONLY for a page that prints a distinct "Grand Total" line separate from "Total"
  -- leave it null otherwise, never duplicate total into it.
- line_items and charges_breakdown are the RAW printed line amounts, in printed order -- you do
  not sum them; that is done downstream.
- amount_in_words is the printed spelled-out amount line verbatim, if the page has one.
```

## User prompt template

```
Transcribe every labeled figure on this document, copying every number and date exactly as
printed. Respond with the JSON object only.
```

(Image attached as `data:image/png;base64,...` per PLAN.md §2.3/§3.)

## Normalization (Rust, `extract::normalize`, NOT the model) — reference for alignment

| Trap | Rule | Image |
|---|---|---|
| Indian lakh/crore grouping | `d,dd,ddd(.dd)` groups are valid when the currency is INR → `2,00,000.00` = 200000 | 02, 10 |
| Comma decimal | Currency USD/EUR with a trailing `,dd` and no other separator → decimal comma → `$33,50` = 33.50 | 12 |
| Trailing `.0` / wrapped cells | Strip a line break inside a number cell, then parse → `9,124.0\n0` = 9124.00 | 03, 15 |
| Currency symbols | `₹`, `Rs`, `Rs.`, `INR`, `Rupees` → INR; `$` → USD; `Rp`, `IDR`, `Rupiahs` → IDR | all |
| Dates | `DD-Mon-YYYY`; `DD/MM/YY` and `DD/MM/YYYY` resolved against the event date | 02, 03, 05, 12 |
| Amount in words | English words to number, including lakh/crore, paise/cents (`extract::witness::words_to_number`) | 01, 05, 06, 08, 09, 10, 16 |

## Deterministic selector (Rust, NOT the model) — reference for engine/extraction alignment

Given the linked event's `event_type` / `category` / `status`:

| Event kind | Selected field (first non-null wins) |
|---|---|
| income, settled/scheduled | `net_pay` → `total` |
| expense, settled | `total` → `amount_paid` |
| expense, scheduled/pending, description implies an existing balance owed | `balance_due` → `amount_due` |
| expense, pending bill with a due-date split | if event `settlement_date` > `due_cutoff_date`, use `amount_due_after_cutoff`; else `amount_due_by_cutoff` (never falls back to the other side, or to `balance_due`/`amount_due`, once a cutoff is known) |

## Witness gate (Rust, `extract::witness`, before the selected figure is trusted)

A figure `F` is accepted only when ALL of:
- two independent reads select the same normalized `F`, and agree on any due-date cutoff;
- at least one witness proves `F`: `line_items` sum, `subtotal + charges_breakdown`,
  `subtotal + tax`, `gross_pay - deductions`, `total - amount_paid = balance_due`,
  `amount_in_words`, or the same final amount repeated under a second final label
  (Total = Grand Total = Amount Payable = Balance);
- no **final-labeled** figure (Total, Grand Total, Amount Due, Balance Due, Net Pay) contradicts
  `F`. A detailed breakdown that does NOT sum to `F` is never itself a contradiction — pages are
  often cut off (image_11) — it simply contributes no witness of its own.

Otherwise the image fails closed. A rejected figure is not zero and is not guessed; it is
surfaced as unresolved evidence (see image_04 in `docs/gold_subset.json`: a delivery-app
screenshot whose total is cropped out).
