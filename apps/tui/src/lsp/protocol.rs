//! The minimal LSP wire types we consume/produce (F-014 slice 1), via serde. Only the fields we use are
//! modelled; unknown fields are ignored. Raw protocol stays inside `lsp/` — [`to_diags`] converts an incoming
//! `publishDiagnostics` into the normalized [`Diag`] model the UI reads.

use serde::Deserialize;
use serde_json::{json, Value};

use super::model::{lsp_pos_to_byte, Diag, Severity};

#[derive(Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Deserialize)]
pub struct LspDiagnostic {
    pub range: LspRange,
    #[serde(default)]
    pub severity: Option<u8>,
    #[serde(default)]
    pub message: String,
}

#[derive(Deserialize)]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    pub diagnostics: Vec<LspDiagnostic>,
}

/// Convert an LSP `publishDiagnostics` payload to normalized byte-range [`Diag`]s against `bytes`.
pub fn to_diags(bytes: &[u8], params: &PublishDiagnosticsParams) -> Vec<Diag> {
    params
        .diagnostics
        .iter()
        .map(|d| {
            let start = lsp_pos_to_byte(bytes, d.range.start.line, d.range.start.character);
            let end = lsp_pos_to_byte(bytes, d.range.end.line, d.range.end.character).max(start);
            Diag {
                start,
                end,
                severity: Severity::from_lsp(d.severity),
                message: d.message.clone(),
            }
        })
        .collect()
}

// --- outgoing param builders (the JSON-RPC envelope is added by the client) ---

/// `initialize` params: announce the workspace root and the (minimal) client capabilities.
pub fn initialize_params(root_uri: &str) -> Value {
    json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {
            "textDocument": {
                "publishDiagnostics": { "relatedInformation": false },
                "synchronization": { "didSave": false }
            }
        }
    })
}

/// `textDocument/didOpen` params.
pub fn did_open_params(uri: &str, language_id: &str, version: i64, text: &str) -> Value {
    json!({
        "textDocument": {
            "uri": uri,
            "languageId": language_id,
            "version": version,
            "text": text
        }
    })
}

/// `textDocument/didChange` params — full-document sync (the whole new text as one change).
pub fn did_change_params(uri: &str, version: i64, text: &str) -> Value {
    json!({
        "textDocument": { "uri": uri, "version": version },
        "contentChanges": [ { "text": text } ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_publish_diagnostics_into_byte_ranges() {
        let bytes = b"fn main() {\n    let x = 1\n}\n"; // missing ';' on line 1
        let raw = json!({
            "uri": "file:///x.rs",
            "diagnostics": [{
                "range": { "start": {"line": 1, "character": 13}, "end": {"line": 1, "character": 13} },
                "severity": 1,
                "message": "expected `;`"
            }]
        });
        let params: PublishDiagnosticsParams = serde_json::from_value(raw).unwrap();
        let diags = to_diags(bytes, &params);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].message, "expected `;`");
        // line 1 col 13 = end of "    let x = 1" (13 chars) → the byte just past the '1'.
        let line1 = bytes.iter().position(|&c| c == b'\n').unwrap() + 1;
        assert_eq!(diags[0].start, line1 + 13);
    }

    #[test]
    fn missing_severity_defaults_to_error() {
        let raw = json!({
            "uri": "file:///x.rs",
            "diagnostics": [{
                "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} },
                "message": "oops"
            }]
        });
        let params: PublishDiagnosticsParams = serde_json::from_value(raw).unwrap();
        assert_eq!(to_diags(b"x", &params)[0].severity, Severity::Error);
    }
}
