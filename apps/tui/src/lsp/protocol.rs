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

/// `textDocument/hover` and `textDocument/definition` share the `{ textDocument, position }` shape.
pub fn position_params(uri: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    })
}

/// `textDocument/formatting` params: the document + the editor's indent options.
pub fn formatting_params(uri: &str, tab_size: u32, insert_spaces: bool) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "options": { "tabSize": tab_size, "insertSpaces": insert_spaces }
    })
}

/// One parsed `TextEdit`: `((start_line, start_char), (end_line, end_char), newText)` in LSP UTF-16 positions.
pub type LspTextEdit = ((u32, u32), (u32, u32), String);

/// Parse a formatting response (a `TextEdit[]`) into [`LspTextEdit`]s the session maps to byte ranges. Empty /
/// non-array → no edits.
pub fn parse_text_edits(result: &Value) -> Vec<LspTextEdit> {
    result
        .as_array()
        .map(|a| parse_edit_array(a))
        .unwrap_or_default() // `map(parse_edit_array)` won't coerce &Vec<Value> → &[Value] here
}

/// Parse a `TextEdit[]` JSON array into [`LspTextEdit`]s (shared by formatting and rename).
fn parse_edit_array(arr: &[Value]) -> Vec<LspTextEdit> {
    arr.iter()
        .filter_map(|e| {
            let range = e.get("range")?;
            let s = range.get("start")?;
            let end = range.get("end")?;
            let text = e.get("newText")?.as_str()?.to_string();
            Some((
                (
                    s.get("line")?.as_u64()? as u32,
                    s.get("character")?.as_u64()? as u32,
                ),
                (
                    end.get("line")?.as_u64()? as u32,
                    end.get("character")?.as_u64()? as u32,
                ),
                text,
            ))
        })
        .collect()
}

/// `textDocument/rename` params: the symbol position + the new name.
pub fn rename_params(uri: &str, line: u32, character: u32, new_name: &str) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
        "newName": new_name
    })
}

/// Parse a `WorkspaceEdit` (a rename response) into per-file edits: `[(uri, TextEdit[])]`. Handles both the
/// `changes: { uri: TextEdit[] }` map and the `documentChanges: [{ textDocument: { uri }, edits: [...] }]`
/// array forms. Resource operations (rename/create/delete files, which lack an `edits` array) are skipped —
/// this slice only rewrites text. Empty / null → no edits.
pub fn parse_workspace_edit(result: &Value) -> Vec<(String, Vec<LspTextEdit>)> {
    if let Some(dcs) = result.get("documentChanges").and_then(Value::as_array) {
        return dcs
            .iter()
            .filter_map(|dc| {
                let uri = dc.get("textDocument")?.get("uri")?.as_str()?.to_string();
                let edits = parse_edit_array(dc.get("edits")?.as_array()?);
                (!edits.is_empty()).then_some((uri, edits))
            })
            .collect();
    }
    if let Some(map) = result.get("changes").and_then(Value::as_object) {
        return map
            .iter()
            .filter_map(|(uri, edits)| {
                let edits = parse_edit_array(edits.as_array()?);
                (!edits.is_empty()).then_some((uri.clone(), edits))
            })
            .collect();
    }
    Vec::new()
}

/// `textDocument/completion` params (the `{ textDocument, position }` shape; context is optional and omitted).
pub fn completion_params(uri: &str, line: u32, character: u32) -> Value {
    position_params(uri, line, character)
}

/// `textDocument/codeAction` params: a zero-width range at the cursor + the diagnostics overlapping it
/// (so the server offers their quickfixes, alongside cursor-position assists/refactors). `diagnostics` is
/// the LSP `Diagnostic[]` the caller reconstructs from its normalized model.
pub fn code_action_params(uri: &str, line: u32, character: u32, diagnostics: Value) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": line, "character": character },
            "end": { "line": line, "character": character }
        },
        "context": { "diagnostics": diagnostics }
    })
}

/// One code action the picker shows/applies: its `title`, the inline `WorkspaceEdit` it makes, and/or a
/// server `command` (id + arguments) to run via `workspace/executeCommand` (whose effect returns as a
/// server `workspace/applyEdit`). An action has at least one of `edit`/`command`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeAction {
    pub title: String,
    pub edit: Vec<(String, Vec<LspTextEdit>)>,
    /// `(command id, arguments)` when the action runs a server command; `None` for a pure edit.
    pub command: Option<(String, Vec<Value>)>,
}

/// Parse a `textDocument/codeAction` response (a `(Command | CodeAction)[]`) into actionable actions — those
/// with an inline `WorkspaceEdit` and/or a `command` (a bare `Command` object counts as command-only). An
/// action with NEITHER is dropped. Empty / null → none.
pub fn parse_code_actions(result: &Value) -> Vec<CodeAction> {
    let Some(arr) = result.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|a| {
            let title = a.get("title")?.as_str()?.to_string();
            let edit = a.get("edit").map(parse_workspace_edit).unwrap_or_default();
            // A CodeAction's `command` is an object; a bare `Command` action IS that object.
            let cmd_obj = a.get("command").filter(|c| c.is_object()).unwrap_or(a);
            let command = cmd_obj.get("command").and_then(Value::as_str).map(|id| {
                let args = cmd_obj
                    .get("arguments")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                (id.to_string(), args)
            });
            (!edit.is_empty() || command.is_some()).then_some(CodeAction {
                title,
                edit,
                command,
            })
        })
        .collect()
}

/// One completion candidate the pum shows/inserts: `label` (displayed), `insert` (text put into the buffer),
/// an optional `detail` (a type/signature shown dimmed on the right), plus the lazily-`resolve`d extras.
#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label: String,
    /// The text to insert: for a snippet item (`snippet == true`) this is the raw LSP snippet BODY the
    /// accept path expands via [`super::snippet::expand`]; otherwise it is inserted literally.
    pub insert: String,
    pub detail: Option<String>,
    /// Whether `insert` is an LSP snippet body (`insertTextFormat == 2`) needing expansion.
    pub snippet: bool,
    /// Documentation text (filled by `completionItem/resolve`; a docs panel is a follow-up). Stored now.
    pub documentation: Option<String>,
    /// `additionalTextEdits` (per-file), e.g. an auto-import — applied WITH the insert on accept (F-014).
    pub additional: Vec<(String, Vec<LspTextEdit>)>,
    /// The ORIGINAL server item JSON — the `completionItem/resolve` request param (carries `data`).
    pub raw: Value,
    /// Whether this item has already been resolved (so it is not resolved again).
    pub resolved: bool,
}

/// Parse a completion response into [`CompletionItem`]s. Accepts both a `CompletionList` (`{ items: [...] }`)
/// and a bare `CompletionItem[]`. Per item, the inserted text is `textEdit.newText` → `insertText` → `label`,
/// EXCEPT snippet items (`insertTextFormat == 2`) fall back to `label` so raw `${1:…}` never lands in the
/// buffer (snippet expansion is a later slice). Null / empty → no items.
pub fn parse_completion(result: &Value) -> Vec<CompletionItem> {
    let arr = if let Some(items) = result.get("items").and_then(Value::as_array) {
        items
    } else if let Some(arr) = result.as_array() {
        arr
    } else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|it| {
            let label = it.get("label")?.as_str()?.to_string();
            let snippet = it.get("insertTextFormat").and_then(Value::as_u64) == Some(2);
            // Both formats resolve the insert text as textEdit.newText → insertText → label. For a snippet
            // the resolved text is the raw SNIPPET BODY (`${1:…}`) that the accept path expands; for a plain
            // item it is inserted literally.
            let insert = it
                .get("textEdit")
                .and_then(|te| te.get("newText"))
                .and_then(Value::as_str)
                .or_else(|| it.get("insertText").and_then(Value::as_str))
                .map_or_else(|| label.clone(), str::to_string);
            let detail = it.get("detail").and_then(Value::as_str).map(str::to_string);
            Some(CompletionItem {
                label,
                insert,
                detail,
                snippet,
                documentation: documentation_text(it),
                additional: parse_additional_text_edits(it),
                raw: it.clone(),
                resolved: false,
            })
        })
        .collect()
}

/// The documentation text of a completion/resolve item — `documentation` as a plain string or a
/// `MarkupContent { value }` (reusing [`markup_text`]). `None` when absent.
fn documentation_text(item: &Value) -> Option<String> {
    item.get("documentation").and_then(markup_text)
}

/// The `additionalTextEdits` of a completion/resolve item as per-file edits `[(uri, TextEdit[])]`. LSP
/// scopes these to the completed document, so they are attributed to the FOCUSED buffer's uri by the caller;
/// here we return them under a single empty-uri key (the caller maps them to the focused file). Empty → none.
pub fn parse_additional_text_edits(item: &Value) -> Vec<(String, Vec<LspTextEdit>)> {
    let Some(arr) = item.get("additionalTextEdits").and_then(Value::as_array) else {
        return Vec::new();
    };
    let edits = parse_edit_array(arr);
    if edits.is_empty() {
        Vec::new()
    } else {
        vec![(String::new(), edits)]
    }
}

/// Whether `item` carries a `data` field — the signal that the server expects a `completionItem/resolve`
/// round-trip (F-014). Items without `data` are used as-is (the capability-absent fallback).
pub fn has_resolve_data(item: &CompletionItem) -> bool {
    item.raw.get("data").is_some()
}

/// Merge a `completionItem/resolve` RESULT into `item` in place: fill `detail` (when the result supplies one),
/// `documentation`, and `additionalTextEdits`; mark `resolved`. NEVER touches `insert`/`snippet` — the accept
/// path always uses the ORIGINAL insert, so resolve can't duplicate or change the inserted text (F-014). A
/// null / non-object result is a no-op beyond marking resolved (the fallback: keep the original item).
pub fn apply_resolve(item: &mut CompletionItem, resolved: &Value) {
    item.resolved = true;
    if !resolved.is_object() {
        return;
    }
    if let Some(d) = resolved.get("detail").and_then(Value::as_str) {
        item.detail = Some(d.to_string());
    }
    if let Some(doc) = documentation_text(resolved) {
        item.documentation = Some(doc);
    }
    let extra = parse_additional_text_edits(resolved);
    if !extra.is_empty() {
        item.additional = extra;
    }
}

/// The display lines of a hover response, or `None` when the server has nothing. Handles `contents` as a
/// plain string, a `MarkedString`/`MarkupContent` object (`{value}`), or an array of either.
pub fn parse_hover(result: &Value) -> Option<Vec<String>> {
    let text = markup_text(result.get("contents")?)?;
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    (!lines.is_empty()).then_some(lines)
}

fn markup_text(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(o) = v.as_object() {
        return o.get("value").and_then(Value::as_str).map(str::to_string);
    }
    if let Some(arr) = v.as_array() {
        let parts: Vec<String> = arr.iter().filter_map(markup_text).collect();
        return (!parts.is_empty()).then(|| parts.join("\n"));
    }
    None
}

/// The `(uri, line, character)` of one `Location`/`LocationLink` — reading `uri`/`targetUri` +
/// `range`/`targetSelectionRange`/`targetRange`, taking the range START. `None` if the shape is unexpected.
fn parse_location(loc: &Value) -> Option<(String, u32, u32)> {
    let uri = loc
        .get("uri")
        .or_else(|| loc.get("targetUri"))
        .and_then(Value::as_str)?
        .to_string();
    let range = loc
        .get("range")
        .or_else(|| loc.get("targetSelectionRange"))
        .or_else(|| loc.get("targetRange"))?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as u32;
    let character = start.get("character")?.as_u64()? as u32;
    Some((uri, line, character))
}

/// The `(uri, line, character)` of a definition response — the first of a `Location`, a `Location[]`, or a
/// `LocationLink[]`. `None` if empty.
pub fn parse_definition(result: &Value) -> Option<(String, u32, u32)> {
    parse_locations(result).into_iter().next()
}

/// EVERY `(uri, line, character)` of a references/definition response — each `Location`/`LocationLink` in
/// the array, or a single bare `Location`. Empty/null → no locations. Used by `textDocument/references`.
pub fn parse_locations(result: &Value) -> Vec<(String, u32, u32)> {
    if let Some(arr) = result.as_array() {
        arr.iter().filter_map(parse_location).collect()
    } else {
        parse_location(result).into_iter().collect()
    }
}

/// `textDocument/references` params: the symbol position + whether to include its declaration.
pub fn references_params(uri: &str, line: u32, character: u32, include_declaration: bool) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
        "context": { "includeDeclaration": include_declaration }
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
    fn parse_hover_handles_string_markup_and_array() {
        assert_eq!(
            parse_hover(&json!({"contents": "line1\nline2"})),
            Some(vec!["line1".into(), "line2".into()])
        );
        assert_eq!(
            parse_hover(&json!({"contents": {"kind": "markdown", "value": "fn f()"}})),
            Some(vec!["fn f()".into()])
        );
        assert_eq!(
            parse_hover(&json!({"contents": ["a", {"value": "b"}]})),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(parse_hover(&json!({})), None);
        assert_eq!(parse_hover(&Value::Null), None);
    }

    #[test]
    fn parse_text_edits_reads_ranges_and_text() {
        let edits = json!([
            {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4}},"newText":"fn "},
            {"range":{"start":{"line":1,"character":2},"end":{"line":1,"character":2}},"newText":"    "}
        ]);
        let parsed = parse_text_edits(&edits);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], ((0, 0), (0, 4), "fn ".to_string()));
        assert_eq!(parsed[1], ((1, 2), (1, 2), "    ".to_string()));
        assert!(parse_text_edits(&Value::Null).is_empty());
    }

    #[test]
    fn parse_workspace_edit_reads_changes_and_document_changes() {
        let te = |c1, c2, t: &str| json!({"range":{"start":{"line":0,"character":c1},"end":{"line":0,"character":c2}},"newText":t});
        // `changes` map form.
        let changes = json!({"changes": {"file:///a.rs": [te(4, 7, "bar")], "file:///b.rs": [te(0, 3, "bar")]}});
        let mut parsed = parse_workspace_edit(&changes);
        parsed.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "file:///a.rs");
        assert_eq!(parsed[0].1, vec![((0, 4), (0, 7), "bar".to_string())]);
        assert_eq!(parsed[1].0, "file:///b.rs");
        // `documentChanges` array form.
        let docs = json!({"documentChanges": [
            {"textDocument": {"uri": "file:///a.rs", "version": 1}, "edits": [te(4, 7, "baz")]}
        ]});
        let parsed = parse_workspace_edit(&docs);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "file:///a.rs");
        assert_eq!(parsed[0].1, vec![((0, 4), (0, 7), "baz".to_string())]);
        // A resource operation (no `edits`) is skipped; null → empty.
        let rename_op = json!({"documentChanges": [{"kind": "rename", "oldUri": "file:///a.rs", "newUri": "file:///c.rs"}]});
        assert!(parse_workspace_edit(&rename_op).is_empty());
        assert!(parse_workspace_edit(&Value::Null).is_empty());
    }

    #[test]
    fn parse_completion_handles_list_array_snippet_and_null() {
        // CompletionList form; insertText overrides label.
        let list = json!({"isIncomplete": false, "items": [
            {"label": "width", "insertText": "width", "detail": "fn width(&self) -> u16"},
            {"label": "with_capacity", "kind": 3}
        ]});
        let items = parse_completion(&list);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "width");
        assert_eq!(items[0].detail.as_deref(), Some("fn width(&self) -> u16"));
        assert_eq!(items[1].insert, "with_capacity"); // no insertText → label
                                                      // Bare array form + a textEdit-driven insert.
        let arr = json!([
            {"label": "foo", "textEdit": {"range": {}, "newText": "foo()"}}
        ]);
        let foo = &parse_completion(&arr)[0];
        assert_eq!(foo.insert, "foo()");
        assert!(!foo.snippet, "a plain item is not a snippet");
        // Snippet item (insertTextFormat 2): keep the raw BODY + flag it (the accept path expands it).
        let snip = json!([
            {"label": "println!", "insertText": "println!(\"$1\")$0", "insertTextFormat": 2}
        ]);
        let s = &parse_completion(&snip)[0];
        assert_eq!(s.insert, "println!(\"$1\")$0");
        assert!(s.snippet);
        assert!(parse_completion(&Value::Null).is_empty());
    }

    #[test]
    fn resolve_merges_extras_without_touching_insert() {
        // An item with `data` is resolvable; one without is used as-is (fallback).
        let items = parse_completion(&json!([
            {"label": "HashMap", "insertText": "HashMap", "data": {"id": 7}},
            {"label": "plain", "insertText": "plain"}
        ]));
        let (mut hm, plain) = (items[0].clone(), items[1].clone());
        assert!(has_resolve_data(&hm));
        assert!(!has_resolve_data(&plain));

        // Resolve fills detail + documentation + additionalTextEdits (auto-import), never the insert text.
        let resolved = json!({
            "label": "HashMap",
            "detail": "struct std::collections::HashMap",
            "documentation": {"kind": "markdown", "value": "A hash map."},
            "insertText": "SHOULD_BE_IGNORED",
            "additionalTextEdits": [
                {"range": {"start": {"line":0,"character":0}, "end": {"line":0,"character":0}},
                 "newText": "use std::collections::HashMap;\n"}
            ]
        });
        apply_resolve(&mut hm, &resolved);
        assert!(hm.resolved);
        assert_eq!(
            hm.detail.as_deref(),
            Some("struct std::collections::HashMap")
        );
        assert_eq!(hm.documentation.as_deref(), Some("A hash map."));
        assert_eq!(
            hm.insert, "HashMap",
            "resolve must NOT change the insert text"
        );
        assert_eq!(hm.additional[0].1.len(), 1); // the import edit

        // A null / non-object result is a no-op beyond marking resolved (fallback keeps the original).
        let mut p2 = plain.clone();
        apply_resolve(&mut p2, &Value::Null);
        assert!(p2.resolved);
        assert_eq!(p2.detail, plain.detail);
        assert_eq!(p2.insert, "plain");
    }

    #[test]
    fn parse_definition_handles_location_array_and_link() {
        let loc = json!({"uri":"file:///a.rs","range":{"start":{"line":3,"character":5},"end":{"line":3,"character":9}}});
        assert_eq!(parse_definition(&loc), Some(("file:///a.rs".into(), 3, 5)));
        assert_eq!(
            parse_definition(&json!([loc.clone()])),
            Some(("file:///a.rs".into(), 3, 5))
        );
        let link = json!([{
            "targetUri":"file:///b.rs",
            "targetSelectionRange":{"start":{"line":7,"character":2},"end":{"line":7,"character":8}}
        }]);
        assert_eq!(parse_definition(&link), Some(("file:///b.rs".into(), 7, 2)));
        assert_eq!(parse_definition(&json!([])), None);
        assert_eq!(parse_definition(&Value::Null), None);
    }

    #[test]
    fn parse_code_actions_keeps_edit_and_command_actions() {
        let te = |c1, c2, t: &str| json!({"range":{"start":{"line":3,"character":c1},"end":{"line":3,"character":c2}},"newText":t});
        let result = json!([
            // Edit-only.
            {"title": "Import Foo", "kind": "quickfix",
             "edit": {"changes": {"file:///a.rs": [te(0, 0, "use x::Foo;\n")]}}},
            // Command-only (kept now) — a CodeAction with a command object + arguments.
            {"title": "Run rustfmt", "command": {"title": "fmt", "command": "rust-analyzer.fmt", "arguments": [1, 2]}},
            // A bare Command (command id + arguments at top level) is command-only too.
            {"title": "Reload", "command": "rust-analyzer.reload", "arguments": ["x"]},
            // An action with NEITHER edit nor command is dropped.
            {"title": "Nothing"}
        ]);
        let actions = parse_code_actions(&result);
        assert_eq!(
            actions.len(),
            3,
            "edit-only + command-only(x2) kept; empty dropped"
        );
        assert_eq!(actions[0].title, "Import Foo");
        assert_eq!(actions[0].edit[0].0, "file:///a.rs");
        assert!(actions[0].command.is_none());
        assert_eq!(actions[1].command.as_ref().unwrap().0, "rust-analyzer.fmt");
        assert_eq!(actions[1].command.as_ref().unwrap().1.len(), 2); // arguments
        assert!(actions[1].edit.is_empty());
        assert_eq!(
            actions[2].command.as_ref().unwrap().0,
            "rust-analyzer.reload"
        );
        assert!(parse_code_actions(&Value::Null).is_empty());
        assert!(parse_code_actions(&json!([])).is_empty());
    }

    #[test]
    fn parse_locations_collects_every_reference() {
        let locs = json!([
            {"uri":"file:///a.rs","range":{"start":{"line":1,"character":4},"end":{"line":1,"character":7}}},
            {"uri":"file:///b.rs","range":{"start":{"line":9,"character":0},"end":{"line":9,"character":3}}}
        ]);
        let got = parse_locations(&locs);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], ("file:///a.rs".into(), 1, 4));
        assert_eq!(got[1], ("file:///b.rs".into(), 9, 0));
        // A single bare Location → one entry; null → none.
        assert_eq!(parse_locations(&locs[0]).len(), 1);
        assert!(parse_locations(&Value::Null).is_empty());
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
