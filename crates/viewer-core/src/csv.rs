//! CSV / TSV parsing into a plain table (`headers` + `rows`).

use super::{Decoded, Family, Format, Input};

/// Formats this module handles (see [`crate::Format`]).
pub(crate) const FORMATS: &[Format] = &[Format {
    exts: &["csv", "tsv"],
    family: Family::Text,
    decode: csv_entry,
}];

fn csv_entry(input: Input) -> Decoded {
    // Tab-separated for .tsv, comma otherwise.
    let delim = if input
        .path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tsv"))
    {
        b'\t'
    } else {
        b','
    };
    decode_csv(&input.bytes, delim)
}

/// A decoded table. Intentionally free of view state (filtering, selection):
/// that belongs to the consumer, not to the decoded data.
#[non_exhaustive]
pub struct CsvData {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Cap on materialised rows. The Text-family byte budget bounds the *input*, but
/// a file of tiny cells (e.g. `a,a,…`) would still explode into hundreds of
/// millions of heap `String`s — many GB resident. Stop well before that; a viewer
/// only needs to show a representative slice.
const MAX_ROWS: usize = 500_000;

pub fn decode_csv(bytes: &[u8], delim: u8) -> Decoded {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .flexible(true)
        .has_headers(false)
        .from_reader(bytes);

    let mut records = rdr.records();
    let mut headers: Vec<String> = match records.next() {
        Some(Ok(r)) => r.iter().map(|s| s.to_string()).collect(),
        Some(Err(e)) => return Decoded::Error(format!("CSV non valido:\n{e}")),
        None => return Decoded::Error("File CSV vuoto".into()),
    };

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut width = headers.len();
    let mut truncated = false;
    for rec in records.flatten() {
        if rows.len() >= MAX_ROWS {
            truncated = true;
            break;
        }
        let row: Vec<String> = rec.iter().map(|s| s.to_string()).collect();
        width = width.max(row.len());
        rows.push(row);
    }
    if truncated {
        eprintln!("CSV troncato a {MAX_ROWS} righe (file molto grande)");
    }
    // Normalise every record (header included) to one canonical column count so a
    // consumer can index columns by `headers` without dropping wider rows' cells.
    headers.resize(width, String::new());
    for row in &mut rows {
        row.resize(width, String::new());
    }

    Decoded::Csv(CsvData { headers, rows })
}
