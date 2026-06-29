//! Plain-text, source-code and config files → [`Decoded::Text`].
//!
//! These formats share one decoder and one family: the bytes are shown verbatim
//! (the app renders them in a read-only code editor), so there's no per-language
//! parsing here — only a broad extension table mapping the usual text/code/config
//! suffixes onto the text [`Family`]. Registering them explicitly (rather than
//! leaning on the root's "unknown UTF-8 → text" fallback) is what makes a `.json`
//! or `.py` a first-class file: it gets the text size budget, appears in the open
//! dialog's filter, and is reachable by arrow-key folder navigation.

use super::{Decoded, Family, Format, Input};

/// Text-like formats handled here. Markdown (`.md`) and CSV/TSV are intentionally
/// absent: they have their own decoders producing richer payloads, and the
/// registry's first-match rule would shadow these rows anyway.
pub(crate) const FORMATS: &[Format] = &[Format {
    exts: &[
        // Plain text & docs-as-text
        "txt", "text", "log", "nfo", "rst", "adoc", "asciidoc", "org", "tex", "bib", "srt", "vtt",
        "diff", "patch",
        // Config / data
        "json", "jsonl", "ndjson", "yaml", "yml", "toml", "ini", "cfg", "conf", "properties", "env",
        "xml", "plist", "editorconfig", "gitignore", "gitattributes", "lock",
        // Web / markup / style
        "html", "htm", "xhtml", "css", "scss", "sass", "less",
        // Shell & build
        "sh", "bash", "zsh", "fish", "ps1", "bat", "cmd", "mk", "make", "cmake", "gradle", "dockerfile",
        // Programming languages
        "rs", "py", "pyi", "js", "mjs", "cjs", "jsx", "ts", "tsx", "c", "h", "cc", "cpp", "cxx",
        "hpp", "hh", "cs", "java", "kt", "kts", "go", "rb", "php", "swift", "scala", "lua", "pl",
        "pm", "r", "sql", "dart", "ex", "exs", "erl", "hrl", "hs", "clj", "cljs", "vim", "asm", "s",
        "m", "mm", "f90", "jl", "nim", "zig", "proto", "graphql", "gql",
    ],
    family: Family::Text,
    decode: text_entry,
}];

fn text_entry(input: Input) -> Decoded {
    decode_text(input.bytes)
}

/// Decode bytes as UTF-8 text. A file that isn't valid UTF-8 is reported as an
/// error rather than shown mangled — mirroring the root's binary-file refusal.
pub fn decode_text(bytes: Vec<u8>) -> Decoded {
    match String::from_utf8(bytes) {
        Ok(s) => Decoded::Text(s),
        Err(_) => Decoded::Error("File di testo non in UTF-8 (contenuto binario?)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_decodes_and_binary_errors() {
        match decode_text(b"fn main() {}\n".to_vec()) {
            Decoded::Text(s) => assert!(s.contains("fn main")),
            other => panic!("atteso Text, ottenuto {other:?}", other = variant(&other)),
        }
        match decode_text(vec![0xff, 0xfe, 0x00]) {
            Decoded::Error(_) => {}
            other => panic!("atteso Error, ottenuto {}", variant(&other)),
        }
    }

    fn variant(d: &Decoded) -> &'static str {
        match d {
            Decoded::Text(_) => "Text",
            Decoded::Error(_) => "Error",
            _ => "altro",
        }
    }
}
