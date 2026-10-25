//! Completion in R chunks, from Ark's language server. Ark is shown a hidden
//! R document holding only the chunks' code, each line where it is in the
//! file and everything else blank, so positions match one for one. Outside
//! R chunks, the editor's usual completions apply.

use crate::chunks::{Chunk, parse_chunks};
use crate::runner::ChunkRunner;
use anyhow::Result;
use editor::{CompletionProvider, Editor};
use gpui::{AppContext as _, Context, Entity, Task, WeakEntity, Window};
use language::{Buffer, ToPoint as _};
use project::{Completion, CompletionResponse, CompletionSource};
use r_console::completions::{is_completion_trigger, request_completions, resolve_completions};
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use text::Anchor;

/// The document Ark sees: the R chunks' code at their own lines, other lines blank.
pub fn chunk_document(text: &str, chunks: &[Chunk]) -> String {
    let code_rows: Vec<Range<u32>> = chunks
        .iter()
        .filter(|chunk| chunk.is_r())
        .map(|chunk| chunk.code_rows.clone())
        .collect();
    let mut document = String::with_capacity(text.len());
    for (row, line) in text.split('\n').enumerate() {
        if row > 0 {
            document.push('\n');
        }
        if code_rows.iter().any(|rows| rows.contains(&(row as u32))) {
            document.push_str(line);
        }
    }
    document
}

/// Whether `row` is a line of code in an R chunk.
pub(crate) fn in_r_code(chunks: &[Chunk], row: u32) -> bool {
    chunks
        .iter()
        .any(|chunk| chunk.is_r() && chunk.code_rows.contains(&row))
}

pub struct ChunkCompletionProvider {
    pub runner: WeakEntity<ChunkRunner>,
    /// The editor's own completions, for prose.
    pub fallback: Option<Rc<dyn CompletionProvider>>,
}

impl ChunkCompletionProvider {
    fn ark_server_id(&self, cx: &gpui::App) -> Option<lsp::LanguageServerId> {
        let runner = self.runner.upgrade()?;
        runner
            .read(cx)
            .completion_server(cx)
            .map(|server| server.server_id())
    }
}

impl CompletionProvider for ChunkCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: Anchor,
        trigger: editor::CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let snapshot = buffer.read(cx).snapshot();
        let text = snapshot.text();
        let chunks = parse_chunks(&text);
        let row = buffer_position.to_point(&snapshot).row;
        if !in_r_code(&chunks, row) {
            let Some(fallback) = &self.fallback else {
                return Task::ready(Ok(Vec::new()));
            };
            // Ark reads the whole file as R, so what it offers in prose is noise.
            let ark = self.ark_server_id(cx);
            let completions = fallback.completions(buffer, buffer_position, trigger, window, cx);
            return cx.background_spawn(async move {
                let mut responses = completions.await?;
                if let Some(ark) = ark {
                    for response in &mut responses {
                        response.completions.retain(|completion| {
                            !matches!(&completion.source, CompletionSource::Lsp { server_id, .. } if *server_id == ark)
                        });
                    }
                }
                Ok(responses)
            });
        }
        let Some(runner) = self.runner.upgrade() else {
            return Task::ready(Ok(Vec::new()));
        };
        let document_text = chunk_document(&text, &chunks);
        runner.update(cx, |runner, cx| {
            let Some(server) = runner.completion_server(cx) else {
                return Task::ready(Ok(Vec::new()));
            };
            let Some(document) = runner.completion_document() else {
                return Task::ready(Ok(Vec::new()));
            };
            request_completions(
                server,
                document,
                document_text,
                snapshot,
                buffer_position,
                cx,
            )
        })
    }

    fn resolve_completions(
        &self,
        buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<bool>> {
        let server = self
            .runner
            .upgrade()
            .and_then(|runner| runner.read(cx).completion_server(cx));
        let from_ark = server.as_ref().is_some_and(|server| {
            let completions = completions.borrow();
            completion_indices.iter().any(|index| {
                matches!(
                    completions.get(*index).map(|completion| &completion.source),
                    Some(CompletionSource::Lsp { server_id, .. }) if *server_id == server.server_id()
                )
            })
        });
        match (server, &self.fallback) {
            (Some(server), _) if from_ark => {
                resolve_completions(server, completion_indices, completions, cx)
            }
            (_, Some(fallback)) => {
                fallback.resolve_completions(buffer, completion_indices, completions, cx)
            }
            _ => Task::ready(Ok(false)),
        }
    }

    fn apply_additional_edits_for_completion(
        &self,
        buffer: Entity<Buffer>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        completion_index: usize,
        push_to_history: bool,
        all_commit_ranges: Vec<Range<language::Anchor>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Option<language::Transaction>>> {
        match &self.fallback {
            Some(fallback) => fallback.apply_additional_edits_for_completion(
                buffer,
                completions,
                completion_index,
                push_to_history,
                all_commit_ranges,
                cx,
            ),
            None => Task::ready(Ok(None)),
        }
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        let snapshot = buffer.read(cx).snapshot();
        let row = position.to_point(&snapshot).row;
        if in_r_code(&parse_chunks(&snapshot.text()), row) {
            return is_completion_trigger(text, trigger_in_words);
        }
        self.fallback.as_ref().is_some_and(|fallback| {
            fallback.is_completion_trigger(buffer, position, text, trigger_in_words, cx)
        })
    }

    fn sort_completions(&self) -> bool {
        self.fallback
            .as_ref()
            .is_none_or(|fallback| fallback.sort_completions())
    }

    fn filter_completions(&self) -> bool {
        self.fallback
            .as_ref()
            .is_none_or(|fallback| fallback.filter_completions())
    }

    fn show_snippets(&self) -> bool {
        self.fallback
            .as_ref()
            .is_some_and(|fallback| fallback.show_snippets())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_document_keeps_only_r_code_in_place() {
        let text = "# Title\n\n```{r}\nx <- mtc\n```\n\nProse with a <- b.\n\n```{python}\nimport os\n```\n";
        let document = chunk_document(text, &parse_chunks(text));
        assert_eq!(document, "\n\n\nx <- mtc\n\n\n\n\n\n\n\n");
        assert_eq!(document.split('\n').count(), text.split('\n').count());
    }
}
