//! Office / OpenDocument: spreadsheets to tables, documents/slides to text.

use super::csv::CsvData;
use super::{Decoded, Family, Format, Input};
use std::io::{Cursor, Read};

/// Formats this module handles (see [`crate::Format`]).
pub(crate) const FORMATS: &[Format] = &[
    Format {
        exts: &["xlsx", "xlsm", "xlsb", "xls", "ods"],
        family: Family::Other,
        decode: spreadsheet_entry,
    },
    Format {
        exts: &["docx"],
        family: Family::Other,
        decode: docx_entry,
    },
    Format {
        exts: &["pptx"],
        family: Family::Other,
        decode: pptx_entry,
    },
    Format {
        exts: &["odt", "odp"],
        family: Family::Other,
        decode: odf_entry,
    },
];

fn spreadsheet_entry(input: Input) -> Decoded {
    decode_spreadsheet(&input.bytes)
}
fn docx_entry(input: Input) -> Decoded {
    decode_docx(&input.bytes)
}
fn pptx_entry(input: Input) -> Decoded {
    decode_pptx(&input.bytes)
}
fn odf_entry(input: Input) -> Decoded {
    decode_odf(&input.bytes)
}

/// A spreadsheet workbook: one named [`CsvData`] table per sheet. Sheet
/// selection is a view concern and is left to the consumer.
#[non_exhaustive]
pub struct SheetData {
    pub sheets: Vec<(String, CsvData)>,
}

pub fn decode_spreadsheet(bytes: &[u8]) -> Decoded {
    use calamine::{open_workbook_auto_from_rs, Data, Range, Reader};

    let mut wb = match open_workbook_auto_from_rs(Cursor::new(bytes)) {
        Ok(w) => w,
        Err(e) => return Decoded::Error(format!("Foglio di calcolo non leggibile:\n{e}")),
    };

    let names = wb.sheet_names().to_owned();
    if names.is_empty() {
        return Decoded::Error("Nessun foglio nel file".into());
    }

    let mut sheets: Vec<(String, CsvData)> = Vec::new();
    for name in names {
        let csv = match wb.worksheet_range(&name) {
            Ok(range) => range_to_csv(&range),
            Err(_) => CsvData {
                headers: Vec::new(),
                rows: Vec::new(),
            },
        };
        sheets.push((name, csv));
    }
    return Decoded::Sheets(SheetData { sheets });

    fn range_to_csv(range: &Range<Data>) -> CsvData {
        let mut iter = range.rows();
        let headers: Vec<String> = iter
            .next()
            .map(|r| r.iter().map(|c| c.to_string()).collect())
            .unwrap_or_default();
        let ncols = headers.len();

        let mut rows: Vec<Vec<String>> = Vec::new();
        for r in iter {
            let mut row: Vec<String> = r.iter().map(|c| c.to_string()).collect();
            row.resize(ncols.max(row.len()), String::new());
            rows.push(row);
        }
        CsvData { headers, rows }
    }
}

fn open_zip(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, String> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("Archivio non valido:\n{e}"))
}

fn zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let mut f = archive.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Pull plain text out of an OOXML/ODF part, inserting newlines/tabs on the
/// given structural tags.
fn xml_extract(xml: &str, para_ends: &[&str], breaks: &[&str], tabs: &[&str]) -> String {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(e)) => {
                if let Ok(t) = e.unescape() {
                    out.push_str(&t);
                }
            }
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let n = name.as_ref();
                if breaks.iter().any(|t| t.as_bytes() == n) {
                    out.push('\n');
                }
                if tabs.iter().any(|t| t.as_bytes() == n) {
                    out.push('\t');
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                if para_ends.iter().any(|t| t.as_bytes() == name.as_ref()) {
                    out.push('\n');
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn text_content(text: String) -> Decoded {
    if text.trim().is_empty() {
        Decoded::Text("(nessun testo estratto dal documento)".into())
    } else {
        Decoded::Text(text)
    }
}

pub fn decode_docx(bytes: &[u8]) -> Decoded {
    let mut a = match open_zip(bytes) {
        Ok(a) => a,
        Err(e) => return Decoded::Error(e),
    };
    match zip_entry(&mut a, "word/document.xml") {
        Some(xml) => text_content(xml_extract(&xml, &["w:p"], &["w:br", "w:cr"], &["w:tab"])),
        None => Decoded::Error("DOCX: word/document.xml mancante".into()),
    }
}

pub fn decode_pptx(bytes: &[u8]) -> Decoded {
    let mut a = match open_zip(bytes) {
        Ok(a) => a,
        Err(e) => return Decoded::Error(e),
    };
    let mut slides: Vec<String> = a
        .file_names()
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .map(|s| s.to_string())
        .collect();
    slides.sort_by_key(|n| {
        n.trim_start_matches("ppt/slides/slide")
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(0)
    });

    let mut out = String::new();
    for (i, name) in slides.iter().enumerate() {
        if let Some(xml) = zip_entry(&mut a, name) {
            out.push_str(&format!("──────── Slide {} ────────\n", i + 1));
            out.push_str(xml_extract(&xml, &["a:p"], &["a:br"], &[]).trim());
            out.push_str("\n\n");
        }
    }
    text_content(out)
}

pub fn decode_odf(bytes: &[u8]) -> Decoded {
    let mut a = match open_zip(bytes) {
        Ok(a) => a,
        Err(e) => return Decoded::Error(e),
    };
    match zip_entry(&mut a, "content.xml") {
        Some(xml) => text_content(xml_extract(
            &xml,
            &["text:p", "text:h"],
            &["text:line-break"],
            &["text:tab"],
        )),
        None => Decoded::Error("ODF: content.xml mancante".into()),
    }
}
