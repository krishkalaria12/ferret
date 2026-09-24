//! CSV and TSV: delimiter sniffing, header detection, bounded parsing.
//!
//! Parsed with the `csv` crate, because RFC 4180 quoting — embedded
//! delimiters, embedded newlines, doubled quotes — is precisely the kind of
//! thing that looks like twenty lines of hand-rolled code and is wrong on
//! real data. Only the delimiter *scoring* is local: the frequency-only
//! heuristic gets European semicolon-delimited exports wrong often enough to
//! matter, so candidates are scored on consistency of field count across the
//! first records, parsed with quote handling.

use super::config::{
    TABLE_INITIAL_ROWS, TABLE_MAX_CELL_BYTES, TABLE_MAX_COLUMNS, TABLE_MAX_FIELD_BYTES,
    TABLE_SNIFF_BYTES,
};

/// One parsed row: the header and every record share this shape.
pub type Row = Vec<String>;

/// A bounded sample of a tabular file.
#[derive(Clone, Debug)]
pub struct ParsedTable {
    pub header: Option<Row>,
    /// At most [`TABLE_INITIAL_ROWS`] records; never the whole file.
    pub rows: Vec<Row>,
    pub delimiter: u8,
    /// An incomplete final record at the byte cap was omitted.
    pub omitted_trailing_record: bool,
    /// A field past [`TABLE_MAX_FIELD_BYTES`] was cut.
    pub cut_field: bool,
    /// Columns past [`TABLE_MAX_COLUMNS`] were dropped. Like `cut_field` and
    /// `omitted_trailing_record`, a preview that hides data without saying so
    /// is the failure these flags exist to prevent.
    pub cut_columns: bool,
}

/// Delimiter candidates, in the order ties prefer.
const CANDIDATES: &[u8] = b",\t;|";

/// Score one delimiter: consistency of field counts across the first
/// complete logical records, parsed with quote awareness.
///
/// A consistent 5-field semicolon file beats a comma file whose "fields"
/// vary row to row. Returns the mean field count when at least two records
/// agree, so the caller prefers the most-structured candidate.
fn score_delimiter(sample: &[u8], delimiter: u8) -> Option<usize> {
    let counts = record_field_counts(sample, delimiter, 20);
    if counts.len() < 2 {
        return None;
    }
    let first = counts[0];
    if first == 0 || !counts.iter().all(|count| *count == first) {
        return None;
    }
    Some(first)
}

/// Field counts of up to `limit` complete logical records.
///
/// Quote-aware: a delimiter inside `"..."` does not split, and `""` is an
/// escaped quote. Records cut by the end of the sample are not counted.
///
/// Mirrors the `csv` builder below (`flexible`, `has_headers(false)`,
/// default quoting: `"` special only at field starts, `""` escaped): the one
/// intentional divergence is that the scorer treats `"` as special anywhere
/// in a field, which can only *over*-score a candidate on adversarial input
/// like `a"b,c` — a wrong-delimiter outcome the parser's own consistency
/// still bounds, never a misparse.
fn record_field_counts(sample: &[u8], delimiter: u8, limit: usize) -> Vec<usize> {
    let mut counts = Vec::new();
    let mut fields = 1usize;
    let mut in_quotes = false;
    let mut bytes = sample.iter().peekable();

    while let Some(byte) = bytes.next() {
        if in_quotes {
            if *byte == b'"' {
                if bytes.peek() == Some(&&b'"') {
                    bytes.next();
                } else {
                    in_quotes = false;
                }
            }
            continue;
        }
        match *byte {
            b'"' => in_quotes = true,
            byte if byte == delimiter => fields += 1,
            b'\n' => {
                counts.push(fields);
                if counts.len() >= limit {
                    return counts;
                }
                fields = 1;
            }
            b'\r' => {
                if bytes.peek() == Some(&&b'\n') {
                    bytes.next();
                }
                counts.push(fields);
                if counts.len() >= limit {
                    return counts;
                }
                fields = 1;
            }
            _ => {}
        }
    }
    // A trailing record without a line ending is complete only if the sample
    // is the whole file — which the scorer cannot know. Leave it out: the
    // parser below makes the same call explicitly.
    counts
}

/// Pick the delimiter for `sample`.
///
/// Scores comma, tab, semicolon and pipe; the winner is the consistent
/// candidate with the most fields. Falls back to comma for single-column
/// data rather than failing — a one-column CSV is still a CSV.
pub fn sniff_delimiter(sample: &[u8]) -> u8 {
    let capped = &sample[..sample.len().min(TABLE_SNIFF_BYTES)];
    let mut best = (b',', 0usize);
    for &delimiter in CANDIDATES {
        if let Some(fields) = score_delimiter(capped, delimiter)
            && fields > best.1
        {
            best = (delimiter, fields);
        }
    }
    best.0
}

/// Whether the first row reads as a header.
///
/// Every field non-numeric, and at least one subsequent row has a numeric
/// field in the same column. Reversible in the UI: when uncertain the grid
/// says Column 1…N instead of guessing.
fn is_header(first: &Row, rest: &[Row]) -> bool {
    if first.is_empty() || first.iter().any(|field| is_numeric(field)) {
        return false;
    }
    first.iter().enumerate().any(|(index, _)| {
        rest.iter()
            .any(|row| row.get(index).is_some_and(|field| is_numeric(field)))
    })
}

fn is_numeric(field: &str) -> bool {
    let field = field.trim();
    if field.is_empty() {
        return false;
    }
    // `f64::from_str` accepts `NaN`, `inf` and exponents: a header literally
    // named `NaN` must not read as numeric. Finite decimal/integer text only.
    if matches!(
        field.to_ascii_lowercase().as_str(),
        "nan" | "inf" | "+inf" | "-inf" | "infinity" | "+infinity" | "-infinity"
    ) {
        return false;
    }
    field.parse::<f64>().is_ok_and(f64::is_finite)
}

/// Parse a bounded sample of `text` with `delimiter`.
///
/// At most [`TABLE_INITIAL_ROWS`] records, [`TABLE_MAX_COLUMNS`] columns,
/// [`TABLE_MAX_FIELD_BYTES`] per field and [`TABLE_MAX_CELL_BYTES`] of cell
/// data. An incomplete final record at the byte cap is omitted with
/// [`ParsedTable::omitted_trailing_record`] set — a preview that hides the
/// broken row is worse than useless for the person trying to find it.
pub fn parse(text: &str, delimiter: u8) -> ParsedTable {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .has_headers(false)
        .double_quote(true)
        .quoting(true)
        .from_reader(text.as_bytes());

    let mut rows: Vec<Row> = Vec::new();
    let mut cell_bytes = 0usize;
    let mut cut_field = false;
    let mut cut_columns = false;
    let mut complete = true;

    // Room for one header plus a full sample: the header is removed below,
    // so the data that survives is a complete `TABLE_INITIAL_ROWS`. One extra
    // record past the cap marks truncation without growing the sample.
    for record in reader.records().take(TABLE_INITIAL_ROWS + 2) {
        let record = match record {
            Ok(record) => record,
            Err(_) => {
                complete = false;
                break;
            }
        };
        if rows.len() > TABLE_INITIAL_ROWS {
            complete = false;
            break;
        }
        let mut row: Row = Vec::with_capacity(record.len().min(TABLE_MAX_COLUMNS));
        if record.len() > TABLE_MAX_COLUMNS {
            cut_columns = true;
        }
        for field in record.iter().take(TABLE_MAX_COLUMNS) {
            let kept = if field.len() > TABLE_MAX_FIELD_BYTES {
                cut_field = true;
                // Cut on a character boundary.
                let mut end = TABLE_MAX_FIELD_BYTES;
                while !field.is_char_boundary(end) {
                    end -= 1;
                }
                &field[..end]
            } else {
                field
            };
            cell_bytes += kept.len();
            if cell_bytes > TABLE_MAX_CELL_BYTES {
                complete = false;
                break;
            }
            row.push(kept.to_string());
        }
        if cell_bytes > TABLE_MAX_CELL_BYTES {
            complete = false;
            break;
        }
        rows.push(row);
    }

    let header = if rows.len() >= 2 && is_header(&rows[0], &rows[1..]) {
        Some(rows.remove(0))
    } else {
        None
    };
    // A headerless sample keeps one record too many above: the loop reserves
    // room for a header that never came. Trim it so `rows` is at most
    // `TABLE_INITIAL_ROWS` either way.
    if header.is_none() && rows.len() > TABLE_INITIAL_ROWS {
        rows.truncate(TABLE_INITIAL_ROWS);
        complete = false;
    }

    ParsedTable {
        header,
        rows,
        delimiter,
        omitted_trailing_record: !complete,
        cut_field,
        cut_columns,
    }
}

/// Sort the bounded sample by one column, for the grid view.
///
/// Sample-only by construction: the grid holds at most
/// [`TABLE_INITIAL_ROWS`](super::config::TABLE_INITIAL_ROWS) records and no
/// full-file read exists, so the header says "sample" and sorting says so
/// too. Numeric-aware — a column of numbers sorts as numbers, mixed content
/// falls back to string order with numbers first — and stable, so repeated
/// clicks on one column never scramble ties.
pub fn sort_sample(rows: &[Row], column: usize, ascending: bool) -> Vec<Row> {
    let mut sorted: Vec<Row> = rows.to_vec();
    sort_rows(&mut sorted, column, ascending);
    sorted
}

/// Sort display rows in place by one column, with the same numeric-aware,
/// stable order as [`sort_sample`].
///
/// Generic over the cell so the grid can sort its shared strings without
/// allocating a parallel `String` copy per header click. Missing cells sort
/// last in both directions: the `None` arms are not reversed with the rest.
pub fn sort_rows<Cell: AsRef<str>>(rows: &mut [Vec<Cell>], column: usize, ascending: bool) {
    rows.sort_by(|a, b| {
        let (x, y) = (
            a.get(column).map(AsRef::as_ref),
            b.get(column).map(AsRef::as_ref),
        );
        match (x, y) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(x), Some(y)) => {
                let ordering = compare_values(x, y);
                if ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            }
        }
    });
}

fn compare_values(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) if x.is_finite() && y.is_finite() => x
            .partial_cmp(&y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b)),
        // Numbers sort before text; text sorts as text.
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commas_win_for_comma_data() {
        assert_eq!(sniff_delimiter(b"a,b,c\n1,2,3\n4,5,6\n"), b',');
    }

    #[test]
    fn semicolons_win_for_european_exports() {
        // The frequency-only heuristic sees the commas inside the quoted
        // field and picks wrong; consistency scoring does not.
        let sample = b"name;amount;note\n\"Acme, Inc.\";12,5;paid\n\"Foo; Bar\";3,0;due\n";
        assert_eq!(sniff_delimiter(sample), b';');
    }

    #[test]
    fn tabs_win_for_tsv() {
        assert_eq!(sniff_delimiter(b"a\tb\n1\t2\n3\t4\n"), b'\t');
    }

    #[test]
    fn quoted_newlines_do_not_split_records() {
        let sample = b"a,b\n\"line one\nline two\",2\n3,4\n";
        assert_eq!(sniff_delimiter(sample), b',');
        let table = parse("a,b\n\"line one\nline two\",2\n3,4\n", b',');
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0][0], "line one\nline two");
    }

    #[test]
    fn headers_are_detected_and_reversible() {
        let table = parse("name,age\namy,30\nbo,41\n", b',');
        assert_eq!(table.header, Some(vec!["name".into(), "age".into()]));
        assert_eq!(table.rows.len(), 2);

        // All-numeric first row is data, not a header.
        let data = parse("1,2\n3,4\n", b',');
        assert_eq!(data.header, None);
        assert_eq!(data.rows.len(), 2);
    }

    #[test]
    fn ragged_and_huge_inputs_are_bounded_not_fatal() {
        let mut big = String::from("a,b,c\n");
        for i in 0..TABLE_INITIAL_ROWS + 50 {
            big.push_str(&format!("{i},{i},{i}\n"));
        }
        let table = parse(&big, b',');
        assert_eq!(table.rows.len(), TABLE_INITIAL_ROWS);
        assert!(table.omitted_trailing_record);

        let wide: String = (0..TABLE_MAX_COLUMNS + 10)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join(",")
            + "\n1,2\n";
        let table = parse(&wide, b',');
        // `c0..cN` over a numeric row reads as a header, truncated with the
        // rest of the sample.
        assert_eq!(table.header.as_ref().map(Vec::len), Some(TABLE_MAX_COLUMNS));
        assert_eq!(table.rows[0], vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn oversized_fields_are_cut_with_a_flag() {
        let huge = "x".repeat(TABLE_MAX_FIELD_BYTES + 10);
        let table = parse(&format!("a\n{huge}\n"), b',');
        assert!(table.cut_field);
        assert!(table.rows[1][0].len() <= TABLE_MAX_FIELD_BYTES);
    }

    #[test]
    fn samples_sort_numerically_not_lexically() {
        let rows = vec![
            vec!["amy".to_string(), "30".to_string()],
            vec!["bo".to_string(), "9".to_string()],
            vec!["cy".to_string(), "100".to_string()],
        ];
        let sorted = sort_sample(&rows, 1, true);
        assert_eq!(
            sorted.iter().map(|row| row[1].clone()).collect::<Vec<_>>(),
            vec!["9", "30", "100"],
            "numeric-aware, or ages sort wrong"
        );
        let reversed = sort_sample(&rows, 1, false);
        assert_eq!(reversed[0][1], "100");
    }

    #[test]
    fn sorting_is_stable_and_ragged_safe() {
        let rows = vec![
            vec!["b".to_string()],
            vec!["a".to_string(), "x".to_string(), "y".to_string()],
            vec!["a".to_string()],
        ];
        // Missing cells sort last; ties keep their input order.
        let sorted = sort_sample(&rows, 0, true);
        assert_eq!(sorted[0].len(), 3);
        assert_eq!(sorted[1], vec!["a".to_string()]);
        assert_eq!(sorted[2], vec!["b".to_string()]);
    }
}
