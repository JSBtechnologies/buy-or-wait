//! Parses Unlimited-OCR's raw per-page output (HTML table rows, plus `<|det|>kind [bbox]
//! <|/det|>text` blocks for everything outside a table) into label:value rows for
//! `labels::figures_from_rows` (fleet/specs/ocr_vllm_pipeline.md work item A2). The model
//! transcribes every label and value as printed; this module never decides which figure
//! matters -- it only recovers the (label, value) pairing structure, deterministically.

use chrono::NaiveDate;

/// One recovered label:value pairing, before role assignment (`labels::figures_from_rows`).
/// `amount`/`date` are the raw `value_raw` normalized via `extract::normalize`, so a value
/// that is neither a parseable amount nor a parseable date is `None` for both -- never a
/// guess (e.g. a spelled-out "amount in words" value parses as neither here; `labels.rs`
/// checks `witness::words_to_number` on `value_raw` separately for that case).
#[derive(Debug, Clone, PartialEq)]
pub struct LabeledValue {
    pub page: u32,
    pub label: String,
    pub value_raw: String,
    pub amount: Option<f64>,
    pub date: Option<NaiveDate>,
}

#[derive(Debug, Clone)]
struct DetBlock {
    kind: String,
    bbox: (i64, i64, i64, i64),
    content: String,
}

/// Parses one page's raw OCR text into `LabeledValue` rows. `currency_hint` disambiguates
/// amount grouping (lakh vs. plain thousands) the same way `extract::images` does.
pub fn parse_page(raw: &str, page: u32, currency_hint: Option<&str>) -> Vec<LabeledValue> {
    let blocks = split_det_blocks(raw);
    let mut rows: Vec<(bool, Vec<String>)> = Vec::new();

    // `bool` marks a row as TABLE-sourced -- the cross-row "label wrapped to the next line"
    // continuation below only ever applies within a table (image_05's "Amount due till" is a
    // table row with nothing else on it); a free-text title/header block with no value of its
    // own (e.g. a lone "PAID" heading) must never be treated as a wrapped label glued onto
    // whatever text happens to follow it on the page.
    let mut pending_text_row: Vec<(String, (i64, i64, i64, i64))> = Vec::new();
    fn flush(pending: &mut Vec<(String, (i64, i64, i64, i64))>, rows: &mut Vec<(bool, Vec<String>)>) {
        if pending.is_empty() {
            return;
        }
        let mut taken = std::mem::take(pending);
        taken.sort_by_key(|(_, b)| b.0);
        rows.push((false, taken.into_iter().map(|(t, _)| t).collect()));
    }

    for block in &blocks {
        match block.kind.as_str() {
            "table" => {
                flush(&mut pending_text_row, &mut rows);
                rows.extend(parse_html_table(&block.content).into_iter().map(|r| (true, r)));
            }
            "image" => {
                flush(&mut pending_text_row, &mut rows);
            }
            _ => {
                let text = html_unescape(block.content.trim());
                if text.is_empty() {
                    continue;
                }
                let (y1, y2) = (block.bbox.1, block.bbox.3);
                let overlaps = pending_text_row.iter().any(|(_, b)| ranges_overlap(y1, y2, b.1, b.3));
                if !pending_text_row.is_empty() && !overlaps {
                    flush(&mut pending_text_row, &mut rows);
                }
                pending_text_row.push((text, block.bbox));
            }
        }
    }
    flush(&mut pending_text_row, &mut rows);

    let mut out = Vec::new();
    let mut pending_label: Option<String> = None;
    for (is_table, row) in rows {
        // Whether the row's own LAST cell (before empty cells are dropped below) actually held
        // text. A wide table row can have its final ("Total") column blank (image_06: the total
        // itself was dropped, leaving only SGST/CGST sub-columns) -- in that case the last
        // SURVIVING fragment after filtering is NOT the row's final column and must not be
        // trusted as one (`rightmost_amount_token` in `push_row`). Contrast image_15, whose
        // final column is genuinely populated (its own row total, e.g. 9,580.00).
        let last_cell_present = is_table && row.last().is_some_and(|c| !c.trim().is_empty());
        let mut cells: Vec<String> = row.into_iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
        if cells.is_empty() {
            continue;
        }

        // A cell can hold an "in words" label glued directly to its spelled-out value with NO
        // delimiter at all (image_10: "Total In Words Indian Rupee Seventy-Nine Thousand ...
        // Paise Only", inside a wider multi-cell row) -- `extract_pairs`'s value-shape
        // heuristic only recognizes a NUMERIC value start, so a words value can never be
        // found that way. Pull any such cell out and emit it directly, wherever it sits in
        // the row, before the rest of this row's normal (label, value) extraction runs.
        cells.retain(|cell| {
            if let Some((label, value)) = split_words_marker(cell) {
                push_row(&mut out, page, &label, &value, currency_hint, false);
                false
            } else {
                true
            }
        });
        if cells.is_empty() {
            continue;
        }

        // A pending label only ever comes from a table row (module doc below); a free-text
        // row in between means the expected continuation never came, so it's dropped here,
        // before it can wrongly gate the colon-split/zip checks just below.
        if !is_table {
            pending_label = None;
        }

        // A single free-text OCR block can hold a whole `"Label : value"` line itself (never
        // pre-split into cells the way an HTML table already is) -- e.g. "Total : Seven
        // Hundred Four Rupees and Five Paise Only". `extract_pairs`'s value-shape heuristic
        // (a value fragment starts with a digit) doesn't apply to a spelled-out words value,
        // so this splits and emits the pair directly rather than routing through it.
        if pending_label.is_none() && cells.len() == 1 {
            if let Some((label, value)) = cells[0].split_once(':') {
                if !label.trim().is_empty() && !value.trim().is_empty() {
                    push_row(&mut out, page, label.trim(), value.trim(), currency_hint, false);
                    continue;
                }
            }
        }

        // Multi-value cell zip (fleet/specs/ocr_vllm_pipeline.md work item A2, image_10
        // summary): a 2-cell row whose two cells each split into the SAME number of
        // newline-separated lines is N label:value pairs zipped in order; a count mismatch
        // is left as a single unmapped pair instead (never a guess at the pairing).
        if pending_label.is_none() && cells.len() == 2 {
            let left: Vec<&str> = cells[0].lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            let right: Vec<&str> = cells[1].lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            if left.len() > 1 && left.len() == right.len() {
                for (label, value_raw) in left.into_iter().zip(right) {
                    push_row(&mut out, page, label, value_raw, currency_hint, false);
                }
                continue;
            }
        }

        let mut effective = cells;
        if let Some(pl) = pending_label.take() {
            effective[0] = format!("{pl} {}", effective[0]);
        }

        let pairs = extract_pairs(&effective);
        if pairs.is_empty() {
            if is_table && effective.len() == 1 {
                pending_label = Some(effective[0].clone());
            }
            continue;
        }
        let last_idx = pairs.len() - 1;
        for (i, (label, value_raw)) in pairs.into_iter().enumerate() {
            // Only the LAST pair in the row can possibly run through to the row's own final
            // cell; only there does `last_cell_present` mean anything (see above).
            let allow_rightmost = last_cell_present && i == last_idx;
            push_row(&mut out, page, &label, &value_raw, currency_hint, allow_rightmost);
        }
    }
    out
}

/// `Some((label, value))` when `cell` contains a case-insensitive whole-word `"words"`
/// immediately preceded by `"in"` (i.e. an "... In Words" marker -- "Total In Words", "Amount
/// In Words", "Paid Amount In Words") with non-empty content after it. The marker itself
/// (through "words") becomes the label; everything after is the spelled-out value.
fn split_words_marker(cell: &str) -> Option<(String, String)> {
    let tokens: Vec<(usize, usize)> = token_byte_ranges(cell);
    for (i, &(start, end)) in tokens.iter().enumerate() {
        if i == 0 {
            continue;
        }
        // A trailing colon/punctuation glued to the token itself ("Words:") must not defeat
        // the match -- only the leading alphabetic run of the token is compared.
        let token = &cell[start..end];
        let alpha_end = start + token.find(|c: char| !c.is_alphabetic()).unwrap_or(token.len());
        if !cell[start..alpha_end].eq_ignore_ascii_case("words") {
            continue;
        }
        let (prev_start, prev_end) = tokens[i - 1];
        let prev_token = &cell[prev_start..prev_end];
        let prev_alpha_end = prev_start + prev_token.find(|c: char| !c.is_alphabetic()).unwrap_or(prev_token.len());
        if !cell[prev_start..prev_alpha_end].eq_ignore_ascii_case("in") {
            continue;
        }
        let label = cell[..alpha_end].trim().to_string();
        let value = cell[end..].trim_start_matches(|c: char| c == ':' || c.is_whitespace()).trim().to_string();
        if !value.is_empty() {
            return Some((label, value));
        }
    }
    None
}

/// Byte-range spans of whitespace-separated tokens in `s`.
fn token_byte_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            if let Some(s0) = start.take() {
                ranges.push((s0, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s0) = start {
        ranges.push((s0, s.len()));
    }
    ranges
}

fn push_row(out: &mut Vec<LabeledValue>, page: u32, label: &str, value_raw: &str, currency_hint: Option<&str>, allow_rightmost: bool) {
    let value_raw = value_raw.trim().to_string();
    let amount = crate::extract::normalize::parse_amount(&value_raw, currency_hint, false)
        .or_else(|| join_rs_ps_cells(&value_raw, currency_hint))
        .or_else(|| if allow_rightmost { rightmost_amount_token(&value_raw, currency_hint) } else { None });
    let date = if amount.is_none() { crate::extract::normalize::parse_date(&value_raw) } else { None };
    out.push(LabeledValue { page, label: label.trim().to_string(), value_raw, amount, date });
}

/// User ruling `ruling.total_or_witnessed_sum` point 3 (image_15): a row merged from many
/// numeric table columns (e.g. a per-line-item breakdown with tax sub-columns) fails to parse
/// as one amount as a whole; the correct per-row amount is the LAST (right-most) column. Only
/// used as a last resort after a whole-string parse and a Rs/Ps cell-join both fail, AND only
/// when the caller's `allow_rightmost` confirms the row's own final cell was actually populated
/// (image_06: the final "Total" column was blank -- its dropped value must never be replaced by
/// an earlier SGST/CGST sub-column that happens to survive empty-cell filtering).
fn rightmost_amount_token(value_raw: &str, currency_hint: Option<&str>) -> Option<f64> {
    let parts: Vec<&str> = value_raw.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    crate::extract::normalize::parse_amount(parts[parts.len() - 1], currency_hint, false)
}

/// User ruling `ruling.total_or_witnessed_sum` point 3 (image_14): a Rs/Ps amount split
/// across SEPARATE table cells (`"TOTAL"|"4 543"|"0"`, not one string like normalize.rs's
/// own `"4543 00"` wrapped-cell case) joins into one value when the whole-rupee part parses
/// on its own and the last fragment is a 1-2 digit paise/cents suffix.
fn join_rs_ps_cells(value_raw: &str, currency_hint: Option<&str>) -> Option<f64> {
    let parts: Vec<&str> = value_raw.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let last = parts[parts.len() - 1];
    if last.is_empty() || last.len() > 2 || !last.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let whole_str = parts[..parts.len() - 1].join(" ");
    let whole = crate::extract::normalize::parse_amount(&whole_str, currency_hint, false)?;
    let paise: f64 = last.parse().ok()?;
    let paise = if last.len() == 1 { paise * 10.0 } else { paise };
    Some(whole + paise / 100.0)
}

fn ranges_overlap(a1: i64, a2: i64, b1: i64, b2: i64) -> bool {
    let a_center = (a1 + a2) / 2;
    let b_center = (b1 + b2) / 2;
    (a1..=a2).contains(&b_center) || (b1..=b2).contains(&a_center)
}

/// Splits `<|det|>kind [x1, y1, x2, y2]<|/det|>content` blocks. `content` runs to the next
/// `<|det|>` marker or end of input. Unwraps a `<|ref|>...<|/ref|>` span around content
/// (kept as label text) and drops embedded `<|det|>...<|/det|>` sub-spans within content
/// (their bbox isn't needed once the outer block's own bbox placed this row).
fn split_det_blocks(raw: &str) -> Vec<DetBlock> {
    let mut blocks = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find("<|det|>") {
        rest = &rest[start + "<|det|>".len()..];
        let Some(close) = rest.find("<|/det|>") else { break };
        let header = &rest[..close];
        rest = &rest[close + "<|/det|>".len()..];

        let next = rest.find("<|det|>").unwrap_or(rest.len());
        let content_raw = &rest[..next];
        rest = &rest[next..];

        let (kind, bbox) = parse_det_header(header);
        let content = strip_ref_and_nested_det(content_raw);
        blocks.push(DetBlock { kind, bbox, content });
    }
    blocks
}

fn parse_det_header(header: &str) -> (String, (i64, i64, i64, i64)) {
    let header = header.trim();
    let (kind, coords) = match header.find('[') {
        Some(idx) => (header[..idx].trim().to_string(), &header[idx..]),
        None => (header.to_string(), ""),
    };
    let nums: Vec<i64> = coords
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .filter_map(|n| n.trim().parse().ok())
        .collect();
    let bbox = match nums[..] {
        [x1, y1, x2, y2] => (x1, y1, x2, y2),
        _ => (0, 0, 0, 0),
    };
    (kind, bbox)
}

/// `<|ref|>text<|/ref|>` unwraps to `text`; a nested `<|det|>...<|/det|>` inside content is
/// dropped entirely (its own bbox isn't meaningful nested inside another block's content).
fn strip_ref_and_nested_det(s: &str) -> String {
    let s = s.replace("<|ref|>", "").replace("<|/ref|>", "");
    let mut out = String::new();
    let mut rest = s.as_str();
    loop {
        match rest.find("<|det|>") {
            Some(start) => {
                out.push_str(&rest[..start]);
                rest = &rest[start..];
                if let Some(end) = rest.find("<|/det|>") {
                    rest = &rest[end + "<|/det|>".len()..];
                } else {
                    break;
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out.trim().to_string()
}

fn html_unescape(s: &str) -> String {
    s.replace("<br/>", "\n")
        .replace("<br>", "\n")
        .replace("<BR>", "\n")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Every `<tr>` in `table_html`, each yielding its `<td>`/`<th>` cell texts in order
/// (`colspan`/`rowspan` attributes are ignored -- only the cell's own text matters here; an
/// empty cell a colspan/rowspan left behind is filtered out by the caller).
fn parse_html_table(table_html: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut rest = table_html;
    while let Some(tr_start) = rest.find("<tr") {
        let Some(tr_open_end) = rest[tr_start..].find('>').map(|i| tr_start + i + 1) else { break };
        let Some(tr_close) = rest[tr_open_end..].find("</tr>").map(|i| tr_open_end + i) else { break };
        let tr_body = &rest[tr_open_end..tr_close];
        rows.push(parse_html_cells(tr_body));
        rest = &rest[tr_close + "</tr>".len()..];
    }
    dedupe_repeated_row_blocks(rows)
}

/// Drops a contiguous block of rows that is an exact byte-for-byte repeat of the block
/// immediately before it (image_06: the model's own transcript repeats one item row and its
/// delivery-charge row back-to-back -- `Sr.5`/`Delivery and other charges` each appear twice,
/// identical cell-for-cell, inflating the line-item sum by a duplicate 290.60 and breaking the
/// `amount_in_words` witness on the true sum). Generic on row CONTENT, never an image id or
/// position: only a block that repeats itself verbatim is ever dropped, so legitimately
/// identical charges that are NOT adjacent repeats of the same block (image_06's four separate
/// 1.60 delivery-charge rows, each paired with a distinct item row) are untouched. Checked at
/// decreasing block sizes first so the largest exact repeat wins over a smaller coincidental one.
fn dedupe_repeated_row_blocks(rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    const MAX_BLOCK: usize = 8;
    let n = rows.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let max_block = ((n - i) / 2).min(MAX_BLOCK);
        let repeat = (1..=max_block).rev().find(|&block| rows[i..i + block] == rows[i + block..i + 2 * block]);
        match repeat {
            Some(block) => {
                out.extend_from_slice(&rows[i..i + block]);
                i += 2 * block;
            }
            None => {
                out.push(rows[i].clone());
                i += 1;
            }
        }
    }
    out
}

fn parse_html_cells(tr_body: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut rest = tr_body;
    loop {
        let Some(td_start) = rest.find("<td").or_else(|| rest.find("<th")) else { break };
        let Some(td_open_end) = rest[td_start..].find('>').map(|i| td_start + i + 1) else { break };
        let close_tag = if rest[td_start..].starts_with("<th") { "</th>" } else { "</td>" };
        let Some(td_close) = rest[td_open_end..].find(close_tag).map(|i| td_open_end + i) else { break };
        let cell_html = &rest[td_open_end..td_close];
        cells.push(html_unescape(&strip_tags(cell_html)));
        rest = &rest[td_close + close_tag.len()..];
    }
    cells
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// A fragment that starts a value (after stripping a leading `:`, currency word, or being a
/// bare `-`/`+`/`=` marker) -- the whole row-splitting heuristic below hinges on this being
/// precise: a LABEL can legitimately contain digits (`"Deduction JHT 3.7%"`), so a fragment
/// only counts as a value start when it BEGINS with a digit (after stripping any known
/// currency prefix/marker), never merely "contains" one.
fn looks_like_value_start(fragment: &str) -> bool {
    let f = fragment.trim();
    if f.is_empty() {
        return false;
    }
    if matches!(f, ":" | "-" | "+" | "=" | ":-") {
        return true;
    }
    if let Some(rest) = f.strip_prefix(':') {
        return !rest.trim().is_empty() || true;
    }
    let stripped = strip_leading_currency_word(f);
    stripped.chars().next().is_some_and(|c| c.is_ascii_digit())
}

const CURRENCY_WORDS: &[&str] = &["IDR", "INR", "USD", "RS", "RS.", "RP", "EUR", "ZAR"];
const CURRENCY_SYMBOLS: &[char] = &['$', '₹', '€', '£', '¥'];

/// Strips a leading currency WORD (space-separated, e.g. `"IDR 4,500,000"`) or a bare currency
/// SYMBOL glued directly to the digits with no space at all (e.g. `"$28.50"`, image_12) --
/// `normalize::parse_amount` already strips either shape once it owns the full string; this
/// copy only needs to know a value fragment when it sees one.
fn strip_leading_currency_word(s: &str) -> &str {
    let trimmed = s.trim();
    for word in CURRENCY_WORDS {
        if let Some(rest) = trimmed.strip_prefix(word) {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return rest.trim_start();
            }
        }
    }
    if let Some(c) = trimmed.chars().next() {
        if CURRENCY_SYMBOLS.contains(&c) {
            return trimmed[c.len_utf8()..].trim_start();
        }
    }
    trimmed
}

/// Strips a leading `:`/marker from a value fragment before it's joined into `value_raw` --
/// a bare marker carries no data (`normalize::parse_amount` cannot parse `"= 704.05"`, the
/// `=`/`-`/`+` is not a recognized currency affix).
fn clean_value_fragment(fragment: &str) -> Option<String> {
    let f = fragment.trim();
    if matches!(f, ":" | "-" | "+" | "=" | ":-") {
        return None;
    }
    let f = f.strip_prefix(':').unwrap_or(f).trim();
    if f.is_empty() {
        None
    } else {
        Some(f.to_string())
    }
}

/// Splits an ordered list of text fragments (HTML table cells, or a visually-grouped row of
/// OCR text blocks) into one or more (label, value) pairs -- a physical row can hold TWO
/// side-by-side label:value pairs (e.g. a payslip's Earnings | Deductions columns in one
/// `<tr>`). A state machine: fragments before the first value-shaped one are the label;
/// value-shaped fragments accumulate into the value; the next NON-value-shaped fragment
/// starts a new pair.
fn extract_pairs(fragments: &[String]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut label_buf: Vec<String> = Vec::new();
    let mut value_buf: Vec<String> = Vec::new();
    let mut in_value = false;

    for f in fragments {
        if looks_like_value_start(f) {
            if let Some(v) = clean_value_fragment(f) {
                value_buf.push(v);
            }
            in_value = true;
        } else if in_value {
            if !label_buf.is_empty() && !value_buf.is_empty() {
                pairs.push((label_buf.join(" "), value_buf.join(" ")));
            }
            label_buf = vec![f.clone()];
            value_buf = Vec::new();
            in_value = false;
        } else {
            label_buf.push(f.clone());
        }
    }
    if !label_buf.is_empty() && !value_buf.is_empty() {
        pairs.push((label_buf.join(" "), value_buf.join(" ")));
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_two_cell_html_table_row() {
        let rows = parse_page(
            r#"<|det|>table [0,0,10,10]<|/det|><table><tr><td>Total (Rs)</td><td>704.05</td></tr></table>"#,
            1,
            Some("INR"),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Total (Rs)");
        assert_eq!(rows[0].amount, Some(704.05));
    }

    #[test]
    fn zips_multi_value_cells_only_when_counts_match() {
        let rows = parse_page(
            "<|det|>table [0,0,10,10]<|/det|><table><tr><td>CGST\nSGST</td><td>50.00\n50.00</td></tr></table>",
            1,
            None,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "CGST");
        assert_eq!(rows[0].amount, Some(50.0));
        assert_eq!(rows[1].label, "SGST");
        assert_eq!(rows[1].amount, Some(50.0));
    }

    #[test]
    fn does_not_zip_when_counts_mismatch() {
        let rows = parse_page(
            "<|det|>table [0,0,10,10]<|/det|><table><tr><td>CGST\nSGST</td><td>50.00</td></tr></table>",
            1,
            None,
        );
        // Falls through to the ordinary pair extractor instead of a guessed zip.
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn splits_two_pairs_from_one_payslip_row() {
        let rows = parse_page(
            "<|det|>table [0,0,10,10]<|/det|><table><tr><td>Salary</td><td>: IDR</td><td>4,500,000</td><td>Deduction BPJS Pen 2% Company</td><td>: IDR 90,000</td></tr></table>",
            1,
            Some("IDR"),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "Salary");
        assert_eq!(rows[0].amount, Some(4_500_000.0));
        assert_eq!(rows[1].label, "Deduction BPJS Pen 2% Company");
        assert_eq!(rows[1].amount, Some(90_000.0));
    }

    #[test]
    fn joins_a_label_wrapped_to_the_next_row_for_a_cutoff_date() {
        let rows = parse_page(
            "<|det|>table [0,0,10,10]<|/det|><table><tr><td>Amount due till</td><td></td><td></td></tr><tr><td>06-Feb-2026</td><td>=</td><td>704.05</td></tr></table>",
            1,
            None,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Amount due till 06-Feb-2026");
        assert_eq!(rows[0].amount, Some(704.05));
    }

    #[test]
    fn groups_separate_text_blocks_on_the_same_visual_row() {
        let raw = "<|det|>text [512, 671, 548, 686]<|/det|>Net Pay\
<|det|>text [789, 671, 821, 685]<|/det|>: IDR\
<|det|>text [888, 671, 930, 685]<|/det|>4,365,000";
        let rows = parse_page(raw, 1, Some("IDR"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Net Pay");
        assert_eq!(rows[0].amount, Some(4_365_000.0));
    }

    #[test]
    fn a_lone_text_value_with_no_label_row_is_pending_not_a_pair() {
        // A label-only text row with nothing on its own line never emits a pair by itself.
        let raw = "<|det|>text [0,0,10,10]<|/det|>Amount due till";
        let rows = parse_page(raw, 1, None);
        assert!(rows.is_empty());
    }

    #[test]
    fn amount_in_words_value_is_recovered_as_a_row_with_no_amount() {
        let raw = "<|det|>text [0,0,300,20]<|/det|>Total : Seven Hundred Four Rupees and Five Paise Only";
        let rows = parse_page(raw, 1, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Total");
        assert_eq!(rows[0].value_raw, "Seven Hundred Four Rupees and Five Paise Only");
        assert_eq!(rows[0].amount, None);
    }
}
