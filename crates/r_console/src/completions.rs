//! Completions from the project's Ark language server for R code that isn't
//! an R file: the console input, and R Markdown chunks.
//!
//! Zed never syncs such code to Ark, so it's shown to Ark as a document that
//! exists only in the server, brought up to date before each request. Ark
//! only serves one client, so this reuses the connection Zed already has
//! instead of opening another.

use anyhow::Result;
use editor::{CompletionProvider, Editor};
use gpui::{AppContext as _, Context, Entity, Task, WeakEntity, Window};
use language::{Buffer, CodeLabel, ToOffset as _, ToPointUtf16 as _};
use lsp::{LanguageServer, LanguageServerId};
use project::lsp_store::CompletionDocumentation;
use project::{Completion, CompletionResponse, CompletionSource};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use text::{Anchor, Bias, PointUtf16, Unclipped};

use crate::ConsolePanel;
use runtime::{RSession, SessionState};
use workspace::Workspace;

/// Ark computes completions on R's thread, so they wait for R; never hold
/// the console up for long.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// R code as Ark sees it, in a document only the server has.
pub struct VirtualDocument {
    uri: lsp::Uri,
    /// The server it's open on and its last version. Ark rejects versions that
    /// go backwards, so this only ever increases.
    open_on: Option<(LanguageServerId, i32)>,
}

impl VirtualDocument {
    /// `name` must be unique among the documents of one R session.
    pub fn new(name: &str) -> Result<Self> {
        // A path that is never written, outside any project, so Ark doesn't
        // also find it when indexing the workspace.
        let path = paths::temp_dir().join(format!("rezstudio-{name}.R"));
        let uri = lsp::Uri::from_file_path(&path)
            .map_err(|()| anyhow::anyhow!("invalid virtual document path {}", path.display()))?;
        Ok(Self { uri, open_on: None })
    }

    pub fn uri(&self) -> &lsp::Uri {
        &self.uri
    }

    /// Sends `text` to `server`, opening the document there first if needed.
    pub fn sync(&mut self, server: &LanguageServer, text: String) -> Result<()> {
        match self.open_on {
            Some((server_id, version)) if server_id == server.server_id() => {
                let version = version + 1;
                server.notify::<lsp::notification::DidChangeTextDocument>(
                    lsp::DidChangeTextDocumentParams {
                        text_document: lsp::VersionedTextDocumentIdentifier {
                            uri: self.uri.clone(),
                            version,
                        },
                        content_changes: vec![lsp::TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text,
                        }],
                    },
                )?;
                self.open_on = Some((server_id, version));
            }
            _ => {
                server.notify::<lsp::notification::DidOpenTextDocument>(
                    lsp::DidOpenTextDocumentParams {
                        text_document: lsp::TextDocumentItem {
                            uri: self.uri.clone(),
                            language_id: "r".to_string(),
                            version: 1,
                            text,
                        },
                    },
                )?;
                self.open_on = Some((server.server_id(), 1));
            }
        }
        Ok(())
    }
}

/// The project's Ark language server, while R is free to answer it. Ark
/// computes completions on R's thread, so asking while R runs code would
/// leave the request waiting.
pub fn ark_language_server(
    workspace: &Entity<Workspace>,
    session: &Entity<RSession>,
    cx: &gpui::App,
) -> Option<Arc<LanguageServer>> {
    let session = session.read(cx);
    if *session.state() != SessionState::Idle {
        return None;
    }
    let root = session.project_root();
    let project = workspace.read(cx).project().read(cx);
    let worktree_id = project
        .worktrees(cx)
        .find(|worktree| worktree.read(cx).abs_path().as_ref() == root.as_path())?
        .read(cx)
        .id();
    let lsp_store = project.lsp_store().read(cx);
    let (server_id, _) = lsp_store.language_server_statuses().find(|(_, status)| {
        status.name == runtime::ARK_LANGUAGE_SERVER_NAME && status.worktree == Some(worktree_id)
    })?;
    lsp_store.language_server_for_id(server_id)
}

/// Asks Ark for completions at `buffer_position` in `snapshot`, after showing
/// it `text` as `document`. `text` must put the code at the same positions as
/// `snapshot` does, so Ark's edits apply to the buffer as they are.
pub fn request_completions(
    server: Arc<LanguageServer>,
    document: &mut VirtualDocument,
    text: String,
    snapshot: language::BufferSnapshot,
    buffer_position: Anchor,
    cx: &gpui::App,
) -> Task<Result<Vec<CompletionResponse>>> {
    if let Err(error) = document.sync(&server, text) {
        return Task::ready(Err(error));
    }
    let position = buffer_position.to_point_utf16(&snapshot);
    let params = lsp::CompletionParams {
        text_document_position: lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: document.uri().clone(),
            },
            position: lsp::Position {
                line: position.row,
                character: position.column,
            },
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: Some(lsp::CompletionContext {
            trigger_kind: lsp::CompletionTriggerKind::INVOKED,
            trigger_character: None,
        }),
    };
    cx.background_spawn(async move {
        let response = server
            .request::<lsp::request::Completion>(params, REQUEST_TIMEOUT)
            .await
            .into_response()?;
        let (items, defaults, is_incomplete) = match response {
            None => (Vec::new(), None, false),
            Some(lsp::CompletionResponse::Array(items)) => (items, None, false),
            Some(lsp::CompletionResponse::List(list)) => (
                list.items,
                list.item_defaults.map(Arc::new),
                list.is_incomplete,
            ),
        };
        let server_id = server.server_id();
        let completions = items
            .into_iter()
            .map(|item| {
                to_completion(
                    item,
                    server_id,
                    defaults.clone(),
                    &snapshot,
                    buffer_position,
                )
            })
            .collect();
        Ok(vec![CompletionResponse {
            completions,
            display_options: Default::default(),
            is_incomplete,
        }])
    })
}

/// Fetches the help Ark attaches to completions once they're selected.
pub fn resolve_completions(
    server: Arc<LanguageServer>,
    completion_indices: Vec<usize>,
    completions: Rc<RefCell<Box<[Completion]>>>,
    cx: &mut Context<Editor>,
) -> Task<Result<bool>> {
    let unresolved: Vec<(usize, lsp::CompletionItem)> = {
        let completions = completions.borrow();
        completion_indices
            .into_iter()
            .filter_map(|index| match &completions.get(index)?.source {
                CompletionSource::Lsp {
                    server_id,
                    lsp_completion,
                    resolved: false,
                    ..
                } if *server_id == server.server_id() => {
                    Some((index, lsp_completion.as_ref().clone()))
                }
                _ => None,
            })
            .collect()
    };
    if unresolved.is_empty() {
        return Task::ready(Ok(false));
    }
    cx.spawn(async move |_, _| {
        let mut updated = false;
        for (index, item) in unresolved {
            let Ok(resolved) = server
                .request::<lsp::request::ResolveCompletionItem>(item, REQUEST_TIMEOUT)
                .await
                .into_response()
            else {
                continue;
            };
            let mut completions = completions.borrow_mut();
            let Some(completion) = completions.get_mut(index) else {
                continue;
            };
            if let Some(documentation) = documentation(&resolved) {
                completion.documentation = Some(documentation);
            }
            if let CompletionSource::Lsp {
                lsp_completion,
                resolved: is_resolved,
                ..
            } = &mut completion.source
            {
                **lsp_completion = resolved;
                *is_resolved = true;
            }
            updated = true;
        }
        Ok(updated)
    })
}

/// Whether typing `text` should ask for completions: `$` and `@` extract
/// members and `:` starts `pkg::`.
pub fn is_completion_trigger(text: &str, trigger_in_words: bool) -> bool {
    let Some(character) = text.chars().last() else {
        return false;
    };
    matches!(character, '$' | '@' | ':') || (trigger_in_words && is_identifier_char(character))
}

pub(crate) struct ConsoleCompletionProvider {
    pub(crate) panel: WeakEntity<ConsolePanel>,
}

impl CompletionProvider for ConsoleCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let Some(panel) = self.panel.upgrade() else {
            return Task::ready(Ok(Vec::new()));
        };
        let snapshot = buffer.read(cx).snapshot();
        let text = snapshot.text();
        panel.update(cx, |panel, cx| {
            let Some(server) = panel.completion_server(cx) else {
                return Task::ready(Ok(Vec::new()));
            };
            let Some(document) = panel.console_document.as_mut() else {
                return Task::ready(Ok(Vec::new()));
            };
            request_completions(server, document, text, snapshot, buffer_position, cx)
        })
    }

    fn resolve_completions(
        &self,
        _buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<bool>> {
        let Some(server) = self
            .panel
            .upgrade()
            .and_then(|panel| panel.read(cx).completion_server(cx))
        else {
            return Task::ready(Ok(false));
        };
        resolve_completions(server, completion_indices, completions, cx)
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        text: &str,
        trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        is_completion_trigger(text, trigger_in_words)
    }
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '.' || character == '_'
}

fn to_completion(
    item: lsp::CompletionItem,
    server_id: LanguageServerId,
    defaults: Option<Arc<lsp::CompletionListItemDefaults>>,
    snapshot: &language::BufferSnapshot,
    position: Anchor,
) -> Completion {
    let (replace_range, new_text) = match &item.text_edit {
        Some(lsp::CompletionTextEdit::Edit(edit)) => {
            (anchor_range(edit.range, snapshot), edit.new_text.clone())
        }
        Some(lsp::CompletionTextEdit::InsertAndReplace(edit)) => {
            (anchor_range(edit.replace, snapshot), edit.new_text.clone())
        }
        None => (
            identifier_before(position, snapshot),
            item.insert_text
                .clone()
                .unwrap_or_else(|| item.label.clone()),
        ),
    };
    // Ark names the package a completion comes from, like RStudio's `{base}`.
    let package = item
        .label_details
        .as_ref()
        .and_then(|details| details.description.clone());
    Completion {
        replace_range,
        new_text,
        label: CodeLabel::plain(item.label.clone(), item.filter_text.as_deref()),
        icon_path: None,
        icon_color: None,
        documentation: documentation(&item)
            .or_else(|| package.map(|package| CompletionDocumentation::SingleLine(package.into()))),
        match_start: None,
        snippet_deduplication_key: None,
        confirm: None,
        source: CompletionSource::Lsp {
            insert_range: None,
            server_id,
            lsp_completion: Box::new(item),
            lsp_defaults: defaults,
            resolved: false,
        },
        insert_text_mode: None,
        group: None,
    }
}

fn documentation(item: &lsp::CompletionItem) -> Option<CompletionDocumentation> {
    match item.documentation.as_ref()? {
        lsp::Documentation::String(text) => Some(CompletionDocumentation::MultiLinePlainText(
            text.clone().into(),
        )),
        lsp::Documentation::MarkupContent(content) => Some(match content.kind {
            lsp::MarkupKind::Markdown => {
                CompletionDocumentation::MultiLineMarkdown(content.value.clone().into())
            }
            lsp::MarkupKind::PlainText => {
                CompletionDocumentation::MultiLinePlainText(content.value.clone().into())
            }
        }),
    }
}

fn anchor_range(range: lsp::Range, snapshot: &language::BufferSnapshot) -> std::ops::Range<Anchor> {
    let point = |position: lsp::Position| {
        snapshot.clip_point_utf16(
            Unclipped(PointUtf16::new(position.line, position.character)),
            Bias::Left,
        )
    };
    snapshot.anchor_before(point(range.start))..snapshot.anchor_after(point(range.end))
}

/// The R identifier ending at `position`, which a completion replaces.
fn identifier_before(
    position: Anchor,
    snapshot: &language::BufferSnapshot,
) -> std::ops::Range<Anchor> {
    let end = position.to_offset(snapshot);
    let start = end
        - snapshot
            .reversed_chars_at(end)
            .take_while(|character| is_identifier_char(*character))
            .map(char::len_utf8)
            .sum::<usize>();
    snapshot.anchor_before(start)..position
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_completions_replace_the_identifier_before_the_cursor(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("x <- is.n", cx));
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let position = snapshot.anchor_after(snapshot.len());

        let item = lsp::CompletionItem {
            label: "is.na".to_string(),
            insert_text: Some("is.na($0)".to_string()),
            insert_text_format: Some(lsp::InsertTextFormat::SNIPPET),
            label_details: Some(lsp::CompletionItemLabelDetails {
                detail: None,
                description: Some("{base}".to_string()),
            }),
            ..Default::default()
        };
        let completion = to_completion(item, LanguageServerId(0), None, &snapshot, position);
        let replaced: String = snapshot
            .text_for_range(completion.replace_range.clone())
            .collect();
        assert_eq!(replaced, "is.n");
        assert_eq!(completion.new_text, "is.na($0)");
        assert!(completion.is_snippet());
        assert!(matches!(
            completion.documentation,
            Some(CompletionDocumentation::SingleLine(ref package)) if package.as_ref() == "{base}"
        ));
    }

    #[gpui::test]
    fn test_text_edit_ranges_are_used_when_given(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("mtcars$mp", cx));
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let position = snapshot.anchor_after(snapshot.len());
        let item = lsp::CompletionItem {
            label: "mpg".to_string(),
            text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
                range: lsp::Range::new(lsp::Position::new(0, 7), lsp::Position::new(0, 9)),
                new_text: "mpg".to_string(),
            })),
            ..Default::default()
        };
        let completion = to_completion(item, LanguageServerId(0), None, &snapshot, position);
        let replaced: String = snapshot
            .text_for_range(completion.replace_range.clone())
            .collect();
        assert_eq!(replaced, "mp");
        assert_eq!(completion.new_text, "mpg");
    }
}
