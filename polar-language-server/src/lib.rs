use std::{collections::BTreeMap, str::Split};

use lsp_types::{
    notification::{
        DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidDeleteFiles,
        DidOpenTextDocument, DidSaveTextDocument, Initialized, Notification,
    },
    DeleteFilesParams, Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidOpenTextDocumentParams, FileChangeType, FileDelete, FileEvent,
    Position, PublishDiagnosticsParams, Range, TextDocumentItem, Url,
    VersionedTextDocumentIdentifier,
};
use polar_core::{
    diagnostic::{Diagnostic as PolarDiagnostic, Range as PolarRange},
    polar::Polar,
    sources::Source,
};
use serde_wasm_bindgen::{from_value, to_value};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(s: &str);
}

#[cfg(not(test))]
fn log(s: &str) {
    #[allow(unused_unsafe)]
    unsafe {
        console_log(&("[pls] ".to_owned() + s))
    }
}

#[cfg(test)]
fn log(_: &str) {}

type Documents = BTreeMap<Url, TextDocumentItem>;
type Diagnostics = BTreeMap<Url, PublishDiagnosticsParams>;

#[wasm_bindgen]
pub struct PolarLanguageServer {
    documents: Documents,
    polar: Polar,
    send_diagnostics_callback: js_sys::Function,
}

fn range_from_polar_diagnostic_context(diagnostic: &PolarDiagnostic) -> Range {
    let context = match diagnostic {
        PolarDiagnostic::Error(e) => e.context.as_ref(),
        PolarDiagnostic::Warning(w) => w.context.as_ref(),
    };

    if let Some(PolarRange { start, end }) = context.map(|c| c.range) {
        let start = Position {
            line: start.row as _,
            character: start.column as _,
        };
        let end = Position {
            line: end.row as _,
            character: end.column as _,
        };
        Range { start, end }
    } else {
        Range::default()
    }
}

fn uri_from_polar_diagnostic_context(diagnostic: &PolarDiagnostic) -> Option<Url> {
    let context = match diagnostic {
        PolarDiagnostic::Error(e) => e.context.as_ref(),
        PolarDiagnostic::Warning(w) => w.context.as_ref(),
    };
    if let Some(context) = context {
        if let Some(filename) = context.source.filename.as_ref() {
            match Url::parse(filename) {
                Ok(uri) => return Some(uri),
                Err(err) => {
                    log(&format!(
                        "Url::parse error: {}\n\tFilename: {}\n\tDiagnostic: {}",
                        err, filename, diagnostic
                    ));
                }
            }
        } else {
            log(&format!(
                "source missing filename:\n\t{:?}\n\tDiagnostic: {}",
                context.source, diagnostic
            ));
        }
    } else {
        log(&format!("missing context:\n\t{:?}", diagnostic));
    }
    None
}

fn empty_diagnostics_for_doc(
    (uri, doc): (&Url, &TextDocumentItem),
) -> (Url, PublishDiagnosticsParams) {
    let params = PublishDiagnosticsParams::new(uri.clone(), vec![], Some(doc.version));
    (uri.clone(), params)
}

/// Public API exposed via WASM.
#[wasm_bindgen]
impl PolarLanguageServer {
    #[wasm_bindgen(constructor)]
    pub fn new(send_diagnostics_callback: &js_sys::Function) -> Self {
        console_error_panic_hook::set_once();

        Self {
            documents: BTreeMap::new(),
            polar: Polar::default(),
            send_diagnostics_callback: send_diagnostics_callback.clone(),
        }
    }

    /// Catch-all handler for notifications sent by the LSP client.
    ///
    /// This function receives a notification's `method` and `params` and dispatches to the
    /// appropriate handler function based on `method`.
    #[wasm_bindgen(js_class = PolarLanguageServer, js_name = onNotification)]
    pub fn on_notification(&mut self, method: &str, params: JsValue) {
        match method {
            DidOpenTextDocument::METHOD => {
                let DidOpenTextDocumentParams { text_document } = from_value(params).unwrap();
                let diagnostics = self.on_did_open_text_document(text_document);
                self.send_diagnostics(diagnostics);
            }
            DidChangeTextDocument::METHOD => {
                let params: DidChangeTextDocumentParams = from_value(params).unwrap();

                // Ensure we receive full -- not incremental -- updates.
                assert_eq!(params.content_changes.len(), 1);
                let change = params.content_changes.into_iter().next().unwrap();
                assert!(change.range.is_none());

                let VersionedTextDocumentIdentifier { uri, version } = params.text_document;
                let updated_doc = TextDocumentItem::new(uri, "polar".into(), version, change.text);
                let diagnostics = self.on_did_change_text_document(updated_doc);
                self.send_diagnostics(diagnostics);
            }
            DidChangeWatchedFiles::METHOD => {
                let DidChangeWatchedFilesParams { changes } = from_value(params).unwrap();
                let uris = changes.into_iter().map(|FileEvent { uri, typ }| {
                    assert_eq!(typ, FileChangeType::Deleted); // We only watch for `Deleted` events.
                    uri
                });
                let diagnostics = self.on_did_delete_files(uris.collect());
                self.send_diagnostics(diagnostics);
            }
            DidDeleteFiles::METHOD => {
                let DeleteFilesParams { files } = from_value(params).unwrap();
                let mut uris = vec![];
                for FileDelete { uri } in files {
                    match Url::parse(&uri) {
                        Ok(uri) => uris.push(uri),
                        Err(e) => log(&format!("Failed to parse URI: {}", e)),
                    }
                }
                let diagnostics = self.on_did_delete_files(uris);
                self.send_diagnostics(diagnostics);
            }
            // We don't care when a document is saved -- we already have the updated state thanks
            // to `DidChangeTextDocument`.
            DidSaveTextDocument::METHOD => (),
            // We don't care when a document is closed -- we care about all Polar files in a
            // workspace folder regardless of which ones remain open.
            DidCloseTextDocument::METHOD => (),
            // Nothing to do when we receive the `Initialized` notification.
            Initialized::METHOD => (),
            _ => log(&format!("on_notification {} {:?}", method, params)),
        }
    }
}

/// Individual LSP notification handlers.
impl PolarLanguageServer {
    fn on_did_open_text_document(&mut self, doc: TextDocumentItem) -> Diagnostics {
        if let Some(TextDocumentItem { uri, .. }) = self.upsert_document(doc) {
            log(&format!("reopened tracked doc: {}", uri));
        }
        self.reload_kb()
    }

    fn on_did_change_text_document(&mut self, doc: TextDocumentItem) -> Diagnostics {
        let uri = doc.uri.clone();
        if self.upsert_document(doc).is_none() {
            log(&format!("updated untracked doc: {}", uri));
        }
        self.reload_kb()
    }

    fn on_did_delete_files(&mut self, uris: Vec<Url>) -> Diagnostics {
        let mut diagnostics = Diagnostics::new();

        for uri in uris {
            let mut msg = format!("deleting URI: {}", uri);

            if let Some(removed) = self.remove_document(&uri) {
                let (_, empty_diagnostics) = empty_diagnostics_for_doc((&uri, &removed));
                if diagnostics.insert(uri, empty_diagnostics).is_some() {
                    msg += "\n\tduplicate watched file event";
                }
            } else {
                msg += "\n\tchecking if URI is dir";
                let removed = self.remove_documents_in_dir(&uri);
                if removed.is_empty() {
                    if uri.as_str().ends_with(".polar") {
                        msg += "\n\tcannot remove untracked doc";
                    }
                } else {
                    for (uri, params) in removed {
                        msg += &format!("\n\t\tremoving dir member: {}", uri);
                        if diagnostics.insert(uri, params).is_some() {
                            msg += "\n\t\tduplicate watched file event";
                        }
                    }
                }
            }
            log(&msg);
        }

        diagnostics.append(&mut self.reload_kb());
        diagnostics
    }
}

/// Helper methods.
impl PolarLanguageServer {
    fn upsert_document(&mut self, doc: TextDocumentItem) -> Option<TextDocumentItem> {
        self.documents.insert(doc.uri.clone(), doc)
    }

    fn remove_document(&mut self, uri: &Url) -> Option<TextDocumentItem> {
        self.documents.remove(uri)
    }

    /// Remove tracked docs inside `dir`.
    fn remove_documents_in_dir(&mut self, dir: &Url) -> Diagnostics {
        let (in_dir, not_in_dir): (Documents, Documents) =
            self.documents.clone().into_iter().partition(|(uri, _)| {
                // Zip pair of `Option<Split<char>>`s into `Option<(Split<char>, Split<char>)>`.
                let maybe_segments = dir.path_segments().zip(uri.path_segments());
                // Compare paths (`Split<char>`) by zipping them together and comparing pairwise.
                let compare_paths = |(l, r): (Split<_>, Split<_>)| l.zip(r).all(|(l, r)| l == r);
                // If all path segments match b/w dir & uri, uri is in dir and should be removed.
                maybe_segments.map_or(false, compare_paths)
            });
        // Replace tracked docs w/ docs that aren't in the removed dir.
        self.documents = not_in_dir;
        in_dir.iter().map(empty_diagnostics_for_doc).collect()
    }

    fn send_diagnostics(&self, diagnostics: Diagnostics) {
        let this = &JsValue::null();
        for params in diagnostics.into_values() {
            let params = &to_value(&params).unwrap();
            if let Err(e) = self.send_diagnostics_callback.call1(this, params) {
                log(&format!(
                    "send_diagnostics params:\n\t{:?}\n\tJS error: {:?}",
                    params, e
                ));
            }
        }
    }

    fn empty_diagnostics_for_all_documents(&self) -> Diagnostics {
        self.documents
            .iter()
            .map(empty_diagnostics_for_doc)
            .collect()
    }

    fn document_from_polar_diagnostic_context(
        &self,
        diagnostic: &PolarDiagnostic,
    ) -> Option<TextDocumentItem> {
        uri_from_polar_diagnostic_context(diagnostic).and_then(|uri| {
            if let Some(document) = self.documents.get(&uri) {
                Some(document.clone())
            } else {
                let tracked_docs = self.documents.keys().map(ToString::to_string);
                let tracked_docs = tracked_docs.collect::<Vec<_>>().join(", ");
                log(&format!(
                    "untracked doc: {}\n\tTracked: {}\n\tDiagnostic: {}",
                    uri, tracked_docs, diagnostic
                ));
                None
            }
        })
    }

    /// Create one or more `Diagnostic`s from `polar_core::diagnostic::Diagnostic`s, filtering out
    /// "ignored" diagnostics.
    fn diagnostics_from_polar_diagnostic(
        &self,
        diagnostic: PolarDiagnostic,
    ) -> Vec<(TextDocumentItem, Diagnostic)> {
        use polar_core::error::{ErrorKind::Validation, ValidationError::*};
        use polar_core::warning::ValidationWarning::UnknownSpecializer;

        // Ignore diagnostics that depend on app data.
        match &diagnostic {
            PolarDiagnostic::Error(e) => match e.kind {
                Validation(UnregisteredClass { .. }) | Validation(SingletonVariable { .. }) => {
                    return vec![];
                }
                _ => (),
            },
            PolarDiagnostic::Warning(w) if matches!(w.kind, UnknownSpecializer { .. }) => {
                return vec![];
            }
            _ => (),
        }

        // NOTE(gj): We stringify the error / warning variant instead of the full `PolarError` /
        // `PolarWarning` because we don't want source context as part of the error message.
        let (message, severity) = match &diagnostic {
            PolarDiagnostic::Error(e) => (e.kind.to_string(), DiagnosticSeverity::Error),
            PolarDiagnostic::Warning(w) => (w.kind.to_string(), DiagnosticSeverity::Warning),
        };

        // If the diagnostic applies to a single doc, use it; otherwise, default to emitting a
        // duplicate diagnostic for all docs.
        let docs = self
            .document_from_polar_diagnostic_context(&diagnostic)
            .map_or_else(
                || self.documents.values().cloned().collect(),
                |doc| vec![doc],
            );

        docs.into_iter()
            .map(|doc| {
                let diagnostic = Diagnostic {
                    range: range_from_polar_diagnostic_context(&diagnostic),
                    severity: Some(severity),
                    source: Some("Polar Language Server".to_owned()),
                    message: message.clone(),
                    ..Default::default()
                };
                (doc, diagnostic)
            })
            .collect()
    }

    /// Turn tracked documents into a set of Polar `Source` structs for `Polar::diagnostic_load`.
    fn documents_to_polar_sources(&self) -> Vec<Source> {
        self.documents
            .values()
            .map(|doc| Source {
                filename: Some(doc.uri.to_string()),
                src: doc.text.clone(),
            })
            .collect()
    }

    fn load_documents(&self) -> Vec<PolarDiagnostic> {
        self.polar
            .diagnostic_load(self.documents_to_polar_sources())
    }

    fn get_diagnostics(&self) -> Diagnostics {
        self.load_documents()
            .into_iter()
            .flat_map(|diagnostic| self.diagnostics_from_polar_diagnostic(diagnostic))
            .fold(Diagnostics::new(), |mut acc, (doc, diagnostic)| {
                let params = acc.entry(doc.uri.clone()).or_insert_with(|| {
                    PublishDiagnosticsParams::new(doc.uri, vec![], Some(doc.version))
                });
                params.diagnostics.push(diagnostic);
                acc
            })
    }

    /// Reloads tracked documents into the `KnowledgeBase`, translates `polar-core` diagnostics
    /// into `polar-language-server` diagnostics, and returns a set of diagnostics for publishing.
    ///
    /// NOTE(gj): we republish 'empty' diagnostics for all documents in order to purge stale
    /// diagnostics.
    fn reload_kb(&self) -> Diagnostics {
        self.polar.clear_rules();
        let mut diagnostics = self.empty_diagnostics_for_all_documents();
        diagnostics.extend(self.get_diagnostics());
        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use wasm_bindgen_test::*;

    use super::*;

    #[track_caller]
    fn new_pls() -> PolarLanguageServer {
        let noop = js_sys::Function::new_with_args("_params", "");
        let pls = PolarLanguageServer::new(&noop);
        assert!(pls.reload_kb().is_empty());
        pls
    }

    #[track_caller]
    fn polar_uri(path: &str) -> Url {
        Url::parse(&format!("file:///{}.polar", path)).unwrap()
    }

    #[track_caller]
    fn polar_doc(path: &str, contents: String) -> TextDocumentItem {
        TextDocumentItem::new(polar_uri(path), "polar".to_owned(), 0, contents)
    }

    #[track_caller]
    fn doc_with_no_errors(path: &str) -> TextDocumentItem {
        let file_name = path.split('/').last().unwrap();
        polar_doc(path, format!("{}();", file_name))
    }

    #[track_caller]
    fn doc_with_missing_semicolon(path: &str) -> TextDocumentItem {
        let file_name = path.split('/').last().unwrap();
        polar_doc(path, format!("{}()", file_name))
    }

    #[track_caller]
    fn add_doc_with_no_errors(pls: &mut PolarLanguageServer, path: &str) -> TextDocumentItem {
        let doc = doc_with_no_errors(path);
        assert!(pls.upsert_document(doc.clone()).is_none());
        doc
    }

    #[track_caller]
    fn add_doc_with_missing_semicolon(
        pls: &mut PolarLanguageServer,
        path: &str,
    ) -> TextDocumentItem {
        let doc = doc_with_missing_semicolon(path);
        assert!(pls.upsert_document(doc.clone()).is_none());
        doc
    }

    #[track_caller]
    fn update_text(doc: TextDocumentItem, text: &str) -> TextDocumentItem {
        TextDocumentItem::new(doc.uri, doc.language_id, doc.version + 1, text.into())
    }

    #[track_caller]
    fn assert_missing_semicolon_error(diagnostics: &Diagnostics, docs: Vec<&TextDocumentItem>) {
        for doc in docs {
            let params = diagnostics.get(&doc.uri).unwrap();
            assert_eq!(params.uri, doc.uri);
            assert_eq!(params.version.unwrap(), doc.version);
            assert_eq!(params.diagnostics.len(), 1, "{}", doc.uri.to_string());
            let diagnostic = params.diagnostics.get(0).unwrap();
            assert_eq!(
                diagnostic.message,
                "hit the end of the file unexpectedly. Did you forget a semi-colon"
            );
        }
    }

    #[track_caller]
    fn assert_no_errors(diagnostics: &Diagnostics, docs: Vec<&TextDocumentItem>) {
        for doc in docs {
            let params = diagnostics.get(&doc.uri).unwrap();
            assert_eq!(params.uri, doc.uri);
            assert_eq!(params.version.unwrap(), doc.version);
            assert!(params.diagnostics.is_empty(), "{:?}", params.diagnostics);
        }
    }

    #[track_caller]
    fn assert_missing_allow_rule_warning(diagnostics: &Diagnostics, docs: Vec<&TextDocumentItem>) {
        for doc in docs {
            let params = diagnostics.get(&doc.uri).unwrap();
            assert_eq!(params.uri, doc.uri);
            assert_eq!(params.version.unwrap(), doc.version);
            assert_eq!(params.diagnostics.len(), 1, "{}", doc.uri.to_string());
            let diagnostic = params.diagnostics.get(0).unwrap();
            let expected = diagnostic
                .message
                .starts_with("Your policy does not contain an allow rule");
            assert!(expected, "{}", diagnostic.message);
        }
    }

    #[allow(clippy::many_single_char_names)]
    #[wasm_bindgen_test]
    fn test_on_did_open_text_document() {
        let mut pls = new_pls();

        let a = doc_with_no_errors("apple");
        let b = doc_with_no_errors("banana");
        let c = doc_with_missing_semicolon("canteloupe");
        let d = doc_with_missing_semicolon("date");
        let e = doc_with_no_errors("elderberry");

        // Load a single doc w/ no errors.
        let diagnostics = pls.on_did_open_text_document(a.clone());
        assert_eq!(diagnostics.len(), 1);
        assert_missing_allow_rule_warning(&diagnostics, vec![&a]);

        // Load a second doc w/ no errors.
        let diagnostics = pls.on_did_open_text_document(b.clone());
        assert_eq!(diagnostics.len(), 2);
        assert_missing_allow_rule_warning(&diagnostics, vec![&a, &b]);

        // Load a third doc w/ errors.
        let diagnostics = pls.on_did_open_text_document(c.clone());
        assert_eq!(diagnostics.len(), 3);
        // No 'missing allow rule' warnings b/c the parse error halts validation before reaching
        // that check.
        assert_no_errors(&diagnostics, vec![&a, &b]);
        assert_missing_semicolon_error(&diagnostics, vec![&c]);

        // Load a fourth doc w/ errors.
        let diagnostics = pls.on_did_open_text_document(d.clone());
        assert_eq!(diagnostics.len(), 4);
        assert_no_errors(&diagnostics, vec![&a, &b]);
        assert_missing_semicolon_error(&diagnostics, vec![&c, &d]);

        // Load a fifth doc w/ no errors.
        let diagnostics = pls.on_did_open_text_document(e.clone());
        assert_eq!(diagnostics.len(), 5);
        assert_no_errors(&diagnostics, vec![&a, &b, &e]);
        assert_missing_semicolon_error(&diagnostics, vec![&c, &d]);
    }

    #[wasm_bindgen_test]
    fn test_on_did_change_text_document() {
        let mut pls = new_pls();

        // 'Change' untracked doc w/ no errors.
        let a0 = doc_with_no_errors("apple");
        let diagnostics0 = pls.on_did_change_text_document(a0.clone());
        assert_eq!(diagnostics0.len(), 1);
        assert_missing_allow_rule_warning(&diagnostics0, vec![&a0]);

        // Change tracked doc w/o introducing an error.
        let a1 = update_text(a0, "pie();");
        let diagnostics1 = pls.on_did_change_text_document(a1.clone());
        assert_eq!(diagnostics1.len(), 1);
        assert_missing_allow_rule_warning(&diagnostics1, vec![&a1]);

        // Change tracked doc, introducing an error.
        let a2 = update_text(a1, "pie()");
        let diagnostics2 = pls.on_did_change_text_document(a2.clone());
        assert_eq!(diagnostics2.len(), 1);
        assert_missing_semicolon_error(&diagnostics2, vec![&a2]);

        // 'Change' untracked doc, introducing a second error.
        let b3 = doc_with_missing_semicolon("banana");
        let diagnostics3 = pls.on_did_change_text_document(b3.clone());
        assert_eq!(diagnostics3.len(), 2);
        assert_missing_semicolon_error(&diagnostics3, vec![&a2, &b3]);

        // Change tracked doc, fixing an error.
        let a4 = update_text(a2, "pie();");
        let diagnostics4 = pls.on_did_change_text_document(a4.clone());
        assert_eq!(diagnostics4.len(), 2);
        // No 'missing allow rule' warnings b/c the parse error halts validation before reaching
        // that check.
        assert_no_errors(&diagnostics4, vec![&a4]);
        assert_missing_semicolon_error(&diagnostics4, vec![&b3]);

        // Change tracked doc, fixing the last error.
        let b5 = update_text(b3, "split();");
        let diagnostics5 = pls.on_did_change_text_document(b5.clone());
        assert_eq!(diagnostics5.len(), 2);
        assert_missing_allow_rule_warning(&diagnostics5, vec![&a4, &b5]);
    }

    #[wasm_bindgen_test]
    fn test_on_did_delete_files() {
        let mut pls = new_pls();

        // Empty event has no effect.
        let diagnostics0 = pls.on_did_delete_files(vec![]);
        assert!(diagnostics0.is_empty());
        assert!(pls.documents.is_empty());

        // Deleting untracked doc has no effect.
        let events1 = vec![polar_uri("apple")];
        let diagnostics1 = pls.on_did_delete_files(events1);
        assert!(diagnostics1.is_empty());
        assert!(pls.documents.is_empty());

        // Deleting tracked doc w/o error.
        let a2 = add_doc_with_no_errors(&mut pls, "apple");
        let events2 = vec![a2.uri.clone()];
        let diagnostics2 = pls.on_did_delete_files(events2);
        assert_eq!(diagnostics2.len(), 1);
        assert_no_errors(&diagnostics2, vec![&a2]);
        assert!(pls.documents.is_empty());

        // Deleting tracked doc w/ error.
        let a3 = add_doc_with_missing_semicolon(&mut pls, "apple");
        let events3 = vec![a3.uri.clone()];
        let diagnostics3 = pls.on_did_delete_files(events3);
        assert_eq!(diagnostics3.len(), 1);
        assert_no_errors(&diagnostics3, vec![&a3]);
        assert!(pls.documents.is_empty());

        // Deleting tracked doc w/o error; doc w/o error remains.
        let a4 = add_doc_with_no_errors(&mut pls, "apple");
        let b4 = add_doc_with_no_errors(&mut pls, "banana");
        let events4 = vec![a4.uri.clone()];
        let diagnostics4 = pls.on_did_delete_files(events4);
        assert_eq!(diagnostics4.len(), 2);
        assert_no_errors(&diagnostics4, vec![&a4]);
        assert_missing_allow_rule_warning(&diagnostics4, vec![&b4]);
        assert!(pls.remove_document(&b4.uri).is_some());
        assert!(pls.documents.is_empty());

        // Deleting tracked doc w/ error; doc w/o error remains.
        let a5 = add_doc_with_missing_semicolon(&mut pls, "apple");
        let b5 = add_doc_with_no_errors(&mut pls, "banana");
        let events5 = vec![a5.uri.clone()];
        let diagnostics5 = pls.on_did_delete_files(events5);
        assert_eq!(diagnostics5.len(), 2);
        assert_no_errors(&diagnostics4, vec![&a5]);
        assert_missing_allow_rule_warning(&diagnostics5, vec![&b5]);
        assert!(pls.remove_document(&b5.uri).is_some());
        assert!(pls.documents.is_empty());

        // Deleting tracked doc w/o error; doc w/ error remains.
        let a6 = add_doc_with_no_errors(&mut pls, "apple");
        let b6 = add_doc_with_missing_semicolon(&mut pls, "banana");
        let events6 = vec![a6.uri.clone()];
        let diagnostics6 = pls.on_did_delete_files(events6);
        assert_eq!(diagnostics6.len(), 2);
        assert_no_errors(&diagnostics6, vec![&a6]);
        assert_missing_semicolon_error(&diagnostics6, vec![&b6]);
        assert!(pls.remove_document(&b6.uri).is_some());
        assert!(pls.documents.is_empty());

        // Deleting tracked doc w/ error; doc w/ error remains.
        let a7 = add_doc_with_missing_semicolon(&mut pls, "apple");
        let b7 = add_doc_with_missing_semicolon(&mut pls, "banana");
        let events7 = vec![a7.uri.clone()];
        let diagnostics7 = pls.on_did_delete_files(events7);
        assert_eq!(diagnostics7.len(), 2);
        assert_no_errors(&diagnostics7, vec![&a7]);
        assert_missing_semicolon_error(&diagnostics7, vec![&b7]);
        assert!(pls.remove_document(&b7.uri).is_some());
        assert!(pls.documents.is_empty());

        // Deleting multiple docs at once.
        let a8 = add_doc_with_missing_semicolon(&mut pls, "apple");
        let b8 = add_doc_with_missing_semicolon(&mut pls, "banana");
        let c8 = add_doc_with_missing_semicolon(&mut pls, "canteloupe");
        let d8 = add_doc_with_no_errors(&mut pls, "date");
        let e8 = add_doc_with_no_errors(&mut pls, "elderberry");
        let f8 = add_doc_with_no_errors(&mut pls, "fig");
        let events8 = vec![
            a8.uri.clone(),
            b8.uri.clone(),
            d8.uri.clone(),
            e8.uri.clone(),
        ];
        let diagnostics8 = pls.on_did_delete_files(events8);
        assert_eq!(diagnostics8.len(), 6);
        // No 'missing allow rule' warnings b/c the parse error halts validation before reaching
        // that check.
        assert_no_errors(&diagnostics8, vec![&a8, &b8, &d8, &e8, &f8]);
        assert_missing_semicolon_error(&diagnostics8, vec![&c8]);
        assert!(pls.remove_document(&c8.uri).is_some());
        assert!(pls.remove_document(&f8.uri).is_some());
        assert!(pls.documents.is_empty());

        // Deleting directories containing Polar files.
        let a9 = add_doc_with_missing_semicolon(&mut pls, "apple");
        let b9 = add_doc_with_no_errors(&mut pls, "a/b/banana");
        let ca9a = add_doc_with_no_errors(&mut pls, "a/b/c/ca/calabash");
        let ca9b = add_doc_with_no_errors(&mut pls, "a/b/c/ca/canteloupe");
        let ch9 = add_doc_with_no_errors(&mut pls, "a/b/c/ch/cherry");
        let d9 = add_doc_with_no_errors(&mut pls, "a/b/c/d/date");
        let g9a = add_doc_with_no_errors(&mut pls, "a/b/c/d/e/f/g/grape");
        let g9b = add_doc_with_no_errors(&mut pls, "a/b/c/d/e/f/g/grapefruit");

        // Deleting a deeply nested directory.
        let d_dir = Url::parse(d9.uri.as_str().strip_suffix("/date.polar").unwrap()).unwrap();
        let events9a = vec![d_dir];
        assert_eq!(pls.documents.len(), 8);
        let diagnostics9a = pls.on_did_delete_files(events9a);
        assert_eq!(diagnostics9a.len(), 8);
        assert_missing_semicolon_error(&diagnostics9a, vec![&a9]);
        // No 'missing allow rule' warnings b/c the parse error halts validation before reaching
        // that check.
        assert_no_errors(
            &diagnostics9a,
            vec![&b9, &ca9a, &ca9b, &ch9, &d9, &g9a, &g9b],
        );
        assert_eq!(pls.documents.len(), 5);

        // Deleting multiple directories at once.
        let ca_dir = ca9a.uri.as_str().strip_suffix("/calabash.polar");
        let ca_dir = Url::parse(ca_dir.unwrap()).unwrap();
        let ch_dir = ch9.uri.as_str().strip_suffix("/cherry.polar");
        let ch_dir = Url::parse(ch_dir.unwrap()).unwrap();
        let events9b = vec![ca_dir, ch_dir];
        assert_eq!(pls.documents.len(), 5);
        let diagnostics9b = pls.on_did_delete_files(events9b);
        assert_eq!(diagnostics9b.len(), 5);
        assert_missing_semicolon_error(&diagnostics9b, vec![&a9]);
        // No 'missing allow rule' warnings b/c the parse error halts validation before reaching
        // that check.
        assert_no_errors(&diagnostics9b, vec![&b9, &ca9a, &ca9b, &ch9]);
        assert_eq!(pls.documents.len(), 2);

        // Deleting a top-level directory.
        let a_dir = b9.uri.as_str().strip_suffix("/b/banana.polar");
        let a_dir = Url::parse(a_dir.unwrap()).unwrap();
        let events9c = vec![a_dir];
        assert_eq!(pls.documents.len(), 2);
        let diagnostics9c = pls.on_did_delete_files(events9c);
        assert_eq!(diagnostics9c.len(), 2);
        assert_missing_semicolon_error(&diagnostics9c, vec![&a9]);
        // No 'missing allow rule' warnings b/c the parse error halts validation before reaching
        // that check.
        assert_no_errors(&diagnostics9c, vec![&b9]);
        assert_eq!(pls.documents.len(), 1);
        assert!(pls.remove_document(&a9.uri).is_some());
        assert!(pls.documents.is_empty());
    }

    #[wasm_bindgen_test]
    fn test_ignoring_errors_dependent_on_app_data() {
        let mut pls = new_pls();

        let resource_block_unregistered_constant = r#"
            allow(_, _, _) if has_permission(_, _, _);
            has_permission(_: Actor, _: String, _: Resource);
            actor User {}
        "#;
        let doc = polar_doc("whatever", resource_block_unregistered_constant.to_owned());
        pls.upsert_document(doc.clone());

        // `load_documents()` API performs no filtering.
        let polar_diagnostics = pls.load_documents();
        assert_eq!(polar_diagnostics.len(), 2, "{:?}", polar_diagnostics);
        let unknown_specializer = polar_diagnostics.get(0).unwrap();
        let expected_message = "Unknown specializer String at line 3, column 41 of file file:///whatever.polar:\n\t003:             has_permission(_: Actor, _: String, _: Resource);\n\t                                             ^\n";
        assert_eq!(unknown_specializer.to_string(), expected_message);
        let unregistered_class = polar_diagnostics.get(1).unwrap();
        assert!(unregistered_class
            .to_string()
            .starts_with("Unregistered class: User"));

        // `reload_kb()` API filters out diagnostics dependent on app data.
        let diagnostics = pls.reload_kb();
        let params = diagnostics.get(&doc.uri).unwrap();
        assert_eq!(params.uri, doc.uri);
        assert_eq!(params.version.unwrap(), doc.version);
        assert!(params.diagnostics.is_empty(), "{:?}", params.diagnostics);

        let rule_type_unregistered_constant = r#"
            allow(_, _, _);
            type f(a: A);
            f(_: B);
        "#;
        let doc = polar_doc("whatever", rule_type_unregistered_constant.to_owned());
        pls.upsert_document(doc.clone());

        // `load_documents()` API performs no filtering.
        let polar_diagnostics = pls.load_documents();
        assert_eq!(polar_diagnostics.len(), 2, "{:?}", polar_diagnostics);
        let unknown_specializer = polar_diagnostics.get(0).unwrap();
        let expected_message = "Unknown specializer B at line 4, column 18 of file file:///whatever.polar:\n\t004:             f(_: B);\n\t                      ^\n";
        assert_eq!(unknown_specializer.to_string(), expected_message);
        let unregistered_constant = polar_diagnostics.get(1).unwrap();
        let expected_message = "Unregistered class: A";
        assert_eq!(unregistered_constant.to_string(), expected_message);

        // `reload_kb()` API filters out diagnostics dependent on app data.
        let diagnostics = pls.reload_kb();
        let params = diagnostics.get(&doc.uri).unwrap();
        assert_eq!(params.uri, doc.uri);
        assert_eq!(params.version.unwrap(), doc.version);
        assert!(params.diagnostics.is_empty(), "{:?}", params.diagnostics);

        let singleton_variable = "allow(a, _, _);".to_owned();
        let doc = polar_doc("whatever", singleton_variable);
        pls.upsert_document(doc.clone());

        // `load_documents()` API performs no filtering.
        let polar_diagnostics = pls.load_documents();
        assert_eq!(polar_diagnostics.len(), 1, "{:?}", polar_diagnostics);
        let singleton_variable = polar_diagnostics.get(0).unwrap();
        assert!(singleton_variable
            .to_string()
            .starts_with("Singleton variable a is unused or undefined; try renaming to _a or _"));

        // `reload_kb()` API filters out diagnostics dependent on app data.
        let diagnostics = pls.reload_kb();
        let params = diagnostics.get(&doc.uri).unwrap();
        assert_eq!(params.uri, doc.uri);
        assert_eq!(params.version.unwrap(), doc.version);
        assert!(params.diagnostics.is_empty(), "{:?}", params.diagnostics);
    }

    #[wasm_bindgen_test]
    fn test_diagnostic_range() {
        let mut pls = new_pls();
        let debug = "debug";
        let doc = polar_doc("whatever", debug.to_owned());
        pls.upsert_document(doc.clone());
        let diagnostics = pls.reload_kb();
        let params = diagnostics.get(&doc.uri).unwrap();
        assert_eq!(params.uri, doc.uri);
        assert_eq!(params.version.unwrap(), doc.version);
        assert_eq!(params.diagnostics.len(), 1);
        let diagnostic = params.diagnostics.get(0).unwrap();
        assert_eq!(
            diagnostic.message,
            "debug is a reserved Polar word and cannot be used here"
        );
        assert_eq!(diagnostic.range.start, Position::new(0, 0));
        assert_eq!(diagnostic.range.end, Position::new(0, 5));
    }
}
