//! CSV / TSV parsing into a plain table (`headers` + `rows`).

use super::Decoded;

/// A decoded table. Intentionally free of view state (filtering, selection):
/// that belongs to the consumer, not to the decoded data.
#[non_exhaustive]
pub struct CsvData {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

pub fn decode_csv(bytes: &[u8], delim: u8) -> Decoded {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .flexible(true)
        .has_headers(false)
        .from_reader(bytes);

    let mut records = rdr.records();
    let headers: Vec<String> = match records.next() {
        Some(Ok(r)) => r.iter().map(|s| s.to_string()).collect(),
        Some(Err(e)) => return Decoded::Error(format!("CSV non valido:\n{e}")),
        None => return Decoded::Error("File CSV vuoto".into()),
    };
    let ncols = headers.len();

    let mut rows: Vec<Vec<String>> = Vec::new();
    for rec in records.flatten() {
        let mut row: Vec<String> = rec.iter().map(|s| s.to_string()).collect();
        row.resize(ncols.max(row.len()), String::new());
        rows.push(row);
    }

    Decoded::Csv(CsvData { headers, rows })
}
