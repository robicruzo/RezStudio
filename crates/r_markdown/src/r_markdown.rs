//! R Markdown and Quarto documents: finding their code chunks and running
//! them in the project's R session, with each chunk's output under it.

pub mod chunks;
mod completion;
mod output;
mod runner;

pub use completion::chunk_document;

use editor::{Addon, Editor};
use gpui::{App, AppContext as _, Context, Entity, Task, Window, actions};
use language::{BufferEvent, Point};
use runner::ChunkRunner;
use workspace::Workspace;

actions!(
    r_markdown,
    [
        /// Runs the R chunk under the cursor, or only the selected code in it,
        /// showing the output under the chunk.
        RunChunk,
        /// Runs every R chunk above the cursor, in order.
        RunChunksAbove,
        /// Runs every R chunk in the document, in order.
        RunAllChunks,
        /// Removes every chunk's output.
        ClearChunkOutputs,
    ]
);

/// The languages whose chunks run.
const LANGUAGES: [&str; 2] = ["R Markdown", "Quarto"];

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx: &mut Context<Editor>| {
        let Some(window) = window else {
            return;
        };
        if !editor.mode().is_full() {
            return;
        }
        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
            return;
        };
        // The language is often still loading when the editor opens.
        cx.subscribe_in(&buffer, window, |_, _, event, window, cx| {
            if let BufferEvent::LanguageChanged(_) = event {
                attach_runner(cx.entity(), window, cx);
            }
        })
        .detach();
        attach_runner(cx.entity(), window, cx);
    })
    .detach();

    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &RunChunk, window, cx| {
                with_runner(workspace, window, cx, |runner, cursor, window, cx| {
                    runner.run_selection_or_chunk(cursor.selection, cursor.row, window, cx)
                })
            })
            .register_action(|workspace, _: &RunChunksAbove, window, cx| {
                with_runner(workspace, window, cx, |runner, cursor, window, cx| {
                    runner.run_chunks_above(cursor.row, window, cx)
                })
            })
            .register_action(|workspace, _: &RunAllChunks, window, cx| {
                with_runner(workspace, window, cx, |runner, _, window, cx| {
                    runner.run_all_chunks(window, cx)
                })
            })
            .register_action(|workspace, _: &ClearChunkOutputs, window, cx| {
                with_runner(workspace, window, cx, |runner, _, _, cx| {
                    runner.clear_outputs(cx)
                })
            });
    })
    .detach();
}

struct ChunkRunnerAddon(Entity<ChunkRunner>);

impl Addon for ChunkRunnerAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn is_r_markdown(editor: &Editor, cx: &App) -> bool {
    editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).language().cloned())
        .is_some_and(|language| LANGUAGES.contains(&language.name().as_ref()))
}

/// Gives an R Markdown or Quarto editor its runner, once.
fn attach_runner(editor: Entity<Editor>, window: &mut Window, cx: &mut App) {
    // The editor is mid-update here, and the runner reads it as it starts.
    window.defer(cx, move |window, cx| {
        let ready = editor.read_with(cx, |editor, cx| {
            editor.addon::<ChunkRunnerAddon>().is_none() && is_r_markdown(editor, cx)
        });
        let Some(workspace) = editor.read(cx).workspace().filter(|_| ready) else {
            return;
        };
        let runner = cx.new(|cx| ChunkRunner::new(&editor, &workspace, window, cx));
        editor.update(cx, |editor, _| {
            let provider = completion::ChunkCompletionProvider {
                runner: runner.downgrade(),
                // The editor's default: the project's language servers, snippets
                // and words.
                fallback: editor.project().cloned().map(|project| {
                    std::rc::Rc::new(project) as std::rc::Rc<dyn editor::CompletionProvider>
                }),
            };
            editor.set_completion_provider(Some(std::rc::Rc::new(provider)));
            editor.register_addon(ChunkRunnerAddon(runner));
        });
    });
}

/// Asks Ark for the help topic at the cursor, when `editor` shows an R
/// Markdown or Quarto document and the cursor is in an R chunk's code.
pub fn help_topic_at_cursor(
    editor: &Entity<Editor>,
    cx: &mut App,
) -> Option<Task<anyhow::Result<Option<String>>>> {
    let runner = editor.read(cx).addon::<ChunkRunnerAddon>()?.0.clone();
    let (snapshot, position) = r_console::completions::cursor_position(editor, cx)?;
    let text = snapshot.text();
    let chunks = chunks::parse_chunks(&text);
    if !completion::in_r_code(&chunks, position.row) {
        return None;
    }
    let document = chunk_document(&text, &chunks);
    Some(runner.update(cx, |runner, cx| runner.help_topic(document, position, cx)))
}

/// Where the cursor is in the active editor.
struct Cursor {
    row: u32,
    /// The newest selection, in order; empty when nothing is selected.
    selection: std::ops::Range<Point>,
}

/// Calls `f` with the active editor's runner and its cursor.
fn with_runner(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut ChunkRunner, Cursor, &mut Window, &mut Context<ChunkRunner>),
) {
    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
        return;
    };
    let Some(runner) = editor
        .read(cx)
        .addon::<ChunkRunnerAddon>()
        .map(|addon| addon.0.clone())
    else {
        return;
    };
    let cursor = editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let selection = editor.selections.newest::<Point>(&snapshot);
        Cursor {
            row: selection.head().row,
            selection: selection.start..selection.end,
        }
    });
    runner.update(cx, |runner, cx| f(runner, cursor, window, cx));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::RunState;
    use futures::channel::mpsc;
    use gpui::{Focusable as _, TestAppContext, VisualContext as _, VisualTestContext};
    use jupyter_protocol::{
        CommId, CommMsg, CommOpen, ExecuteReply, JupyterMessage, JupyterMessageContent,
        KernelInfoReply, ReplyStatus, Status, Stdio, StreamContent,
    };
    use language::{Language, LanguageConfig, LanguageMatcher};
    use languages::r::{RInstallationSource, RToolchainData};
    use project::{FakeFs, Project};
    use runtime::RSession;
    use runtime::comms::PLOT_COMM_TARGET;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use workspace::MultiWorkspace;

    /// A 1×1 PNG, base64-encoded without padding as Ark sends it.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg";

    const DOCUMENT: &str = "# Report

```{r setup}
x <- 1
```

```{python}
print(1)
```

```{r broken}
stop(\"no\")
```

```{r plot}
plot(x)
```

```{r many}
a <- 1
b <- 2
c <- 3
```
";

    struct Harness {
        editor: Entity<Editor>,
        runner: Entity<ChunkRunner>,
        session: Entity<RSession>,
        requests: mpsc::Receiver<JupyterMessage>,
    }

    impl Harness {
        async fn new(cx: &mut TestAppContext) -> (Self, &mut VisualTestContext) {
            Self::new_with(cx, |_| {}).await
        }

        /// `register` adds language servers before the document opens.
        async fn new_with(
            cx: &mut TestAppContext,
            register: impl FnOnce(&Arc<language::LanguageRegistry>),
        ) -> (Self, &mut VisualTestContext) {
            cx.update(|cx| {
                let settings = SettingsStore::test(cx);
                cx.set_global(settings);
                theme_settings::init(theme::LoadThemes::JustBase, cx);
                editor::init(cx);
                init(cx);
            });
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree("/project", json!({ "report.Rmd": DOCUMENT }))
                .await;
            let project = Project::test(fs, [Path::new("/project")], cx).await;
            project.read_with(cx, |project, _| {
                project.languages().add(Arc::new(Language::new(
                    LanguageConfig {
                        name: "R Markdown".into(),
                        matcher: Arc::new(LanguageMatcher {
                            path_suffixes: vec!["Rmd".into()],
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None,
                )))
            });
            let languages = project.read_with(cx, |project, _| project.languages().clone());
            register(&languages);
            let window =
                cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
            let workspace = window
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                .unwrap();
            let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        PathBuf::from("/project/report.Rmd"),
                        workspace::OpenOptions::default(),
                        window,
                        cx,
                    )
                })
                .await
                .unwrap();
            let editor = item.downcast::<Editor>().unwrap();
            cx.run_until_parked();
            let runner = editor
                .read_with(cx, |editor, _| {
                    editor
                        .addon::<ChunkRunnerAddon>()
                        .map(|addon| addon.0.clone())
                })
                .expect("an R Markdown editor gets a runner");

            let session = cx.new(|cx| {
                RSession::new(
                    Some(RToolchainData {
                        r_home: PathBuf::from("/opt/R/4.5.3/lib/R"),
                        version: Some("4.5.3".to_string()),
                        source: RInstallationSource::Rig,
                    }),
                    PathBuf::from("/project"),
                    Default::default(),
                    None,
                    cx,
                )
            });
            let (requests_tx, mut requests) = mpsc::channel(100);
            let (stdin_tx, _stdin) = mpsc::channel(100);
            session.update(cx, |session, cx| {
                session.connect_for_test(requests_tx, stdin_tx, cx)
            });
            let kernel_info = requests.try_recv().unwrap();
            runner.update_in(cx, |runner, window, cx| {
                runner.set_session(session.clone(), window, cx)
            });
            let harness = Self {
                editor,
                runner,
                session,
                requests,
            };
            let reply: KernelInfoReply = serde_json::from_value(json!({
                "status": "ok", "protocol_version": "5.3", "implementation": "ark",
                "implementation_version": "0.1.252",
                "language_info": { "name": "R", "version": "4.5.3" },
                "banner": "", "help_links": [], "debugger": false
            }))
            .unwrap();
            let mut harness = harness;
            harness.receive(JupyterMessage::new(reply, Some(&kernel_info)), cx);
            while harness.requests.try_recv().is_ok() {}
            (harness, cx)
        }

        fn receive(&self, message: JupyterMessage, cx: &mut VisualTestContext) {
            self.session.update_in(cx, |session, window, cx| {
                session.receive_for_test(&message, window, cx)
            });
            cx.run_until_parked();
        }

        /// The code of the next execute request.
        fn next_execution(&mut self) -> (String, JupyterMessage) {
            loop {
                let request = self.requests.try_recv().expect("an execute request");
                if let JupyterMessageContent::ExecuteRequest(execute) = &request.content {
                    return (execute.code.clone(), request);
                }
            }
        }

        fn no_execution(&mut self) {
            while let Ok(request) = self.requests.try_recv() {
                assert!(
                    !matches!(request.content, JupyterMessageContent::ExecuteRequest(_)),
                    "unexpected execution"
                );
            }
        }

        fn finish(&self, request: &JupyterMessage, succeeded: bool, cx: &mut VisualTestContext) {
            let reply = ExecuteReply {
                status: if succeeded {
                    ReplyStatus::Ok
                } else {
                    ReplyStatus::Error
                },
                ..ExecuteReply::default()
            };
            self.receive(JupyterMessage::new(reply, Some(request)), cx);
            self.receive(JupyterMessage::new(Status::idle(), Some(request)), cx);
        }

        fn outputs(&self, cx: &mut VisualTestContext) -> Vec<(RunState, usize)> {
            let outputs = self
                .runner
                .read_with(cx, |runner, _| runner.outputs_for_test());
            outputs
                .iter()
                .map(|output| {
                    output.read_with(cx, |output, cx| {
                        (output.state().clone(), output.output_count(cx))
                    })
                })
                .collect()
        }

        fn run(
            &self,
            f: impl FnOnce(&mut ChunkRunner, &mut Window, &mut Context<ChunkRunner>),
            cx: &mut VisualTestContext,
        ) {
            self.runner
                .update_in(cx, |runner, window, cx| f(runner, window, cx));
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    async fn test_runs_a_chunk_and_shows_its_output(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        // A run line above each R chunk, none above the Python one.
        assert_eq!(
            harness
                .runner
                .read_with(cx, |runner, _| runner.lens_count()),
            4
        );

        harness.run(|runner, window, cx| runner.run_chunk_at(3, window, cx), cx);
        let (code, request) = harness.next_execution();
        assert_eq!(code, "x <- 1");
        assert!(
            harness
                .session
                .read_with(cx, |session, _| session.is_inline(&request.header.msg_id))
        );
        assert_eq!(harness.outputs(cx), [(RunState::Running, 0)]);
        harness.receive(
            JupyterMessage::new(
                StreamContent {
                    name: Stdio::Stdout,
                    text: "[1] 1\n".into(),
                },
                Some(&request),
            ),
            cx,
        );
        harness.finish(&request, true, cx);
        assert_eq!(
            harness.outputs(cx),
            [(RunState::Finished { succeeded: true }, 1)]
        );

        // Running it again replaces its output.
        harness.run(|runner, window, cx| runner.run_chunk_at(2, window, cx), cx);
        let (_, request) = harness.next_execution();
        assert_eq!(harness.outputs(cx), [(RunState::Running, 0)]);
        harness.finish(&request, true, cx);

        // The Python chunk isn't run.
        harness.run(|runner, window, cx| runner.run_chunk_at(7, window, cx), cx);
        harness.no_execution();

        harness.run(|runner, _, cx| runner.clear_outputs(cx), cx);
        assert!(harness.outputs(cx).is_empty());
    }

    #[gpui::test]
    async fn test_run_all_stops_at_an_error(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.run(|runner, window, cx| runner.run_all_chunks(window, cx), cx);
        // One at a time: the next waits for the one before.
        let (code, first) = harness.next_execution();
        assert_eq!(code, "x <- 1");
        harness.no_execution();
        assert_eq!(
            harness
                .outputs(cx)
                .iter()
                .map(|(state, _)| state.clone())
                .collect::<Vec<_>>(),
            [
                RunState::Running,
                RunState::Queued,
                RunState::Queued,
                RunState::Queued
            ]
        );
        harness.finish(&first, true, cx);
        let (code, second) = harness.next_execution();
        assert_eq!(code, "stop(\"no\")");
        harness.finish(&second, false, cx);
        harness.no_execution();
        assert_eq!(
            harness
                .outputs(cx)
                .iter()
                .map(|(state, _)| state.clone())
                .collect::<Vec<_>>(),
            [
                RunState::Finished { succeeded: true },
                RunState::Finished { succeeded: false },
                RunState::Skipped("Not run: an earlier chunk failed.".into()),
                RunState::Skipped("Not run: an earlier chunk failed.".into()),
            ]
        );

        // Run Chunks Above runs what comes before the cursor's chunk.
        harness.run(
            |runner, window, cx| runner.run_chunks_above(14, window, cx),
            cx,
        );
        let (code, first) = harness.next_execution();
        assert_eq!(code, "x <- 1");
        harness.finish(&first, true, cx);
        let (code, _) = harness.next_execution();
        assert_eq!(code, "stop(\"no\")");
    }

    #[gpui::test]
    async fn test_plots_show_under_their_chunk(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.run(|runner, window, cx| runner.run_chunk_at(14, window, cx), cx);
        let (code, request) = harness.next_execution();
        assert_eq!(code, "plot(x)");
        harness.receive(
            JupyterMessage::new(
                CommOpen {
                    comm_id: CommId("plot-1".into()),
                    target_name: PLOT_COMM_TARGET.into(),
                    data: serde_json::Map::new(),
                    target_module: None,
                },
                None,
            ),
            cx,
        );
        // R draws it at the output's width, in the chunk's shape (7 by 5 by default).
        let render = loop {
            let message = harness.requests.try_recv().expect("a render request");
            if let JupyterMessageContent::CommMsg(comm) = &message.content
                && comm.data["method"] == "render"
            {
                break message;
            }
        };
        let JupyterMessageContent::CommMsg(comm) = &render.content else {
            unreachable!()
        };
        let size = &comm.data["params"]["size"];
        let (width, height) = (
            size["width"].as_i64().unwrap(),
            size["height"].as_i64().unwrap(),
        );
        assert!(width >= 200, "{size}");
        assert_eq!(height, (width as f32 * 5. / 7.).floor() as i64);
        let serde_json::Value::Object(data) = json!({ "method": "render", "result": {
            "data": PNG, "mime_type": "image/png", "settings": null
        }}) else {
            unreachable!()
        };
        harness.receive(
            JupyterMessage::new(
                CommMsg {
                    comm_id: CommId("plot-1".into()),
                    data,
                },
                Some(&render),
            ),
            cx,
        );
        harness.finish(&request, true, cx);
        let output = harness
            .runner
            .read_with(cx, |runner, _| runner.outputs_for_test()[0].clone());
        assert_eq!(output.read_with(cx, |output, _| output.plot_images()), 1);

        // Clearing the output lets R free the plot.
        harness.run(|runner, _, cx| runner.clear_outputs(cx), cx);
        let closed = std::iter::from_fn(|| harness.requests.try_recv().ok())
            .any(|message| matches!(&message.content, JupyterMessageContent::CommClose(close) if close.comm_id.0 == "plot-1"));
        assert!(closed);
    }

    #[gpui::test]
    async fn test_run_chunk_runs_only_the_selection(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        let mut run_with_selection = |range: std::ops::Range<Point>, cx: &mut VisualTestContext| {
            harness.editor.update_in(cx, |editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([range])
                })
            });
            cx.focus(&harness.editor);
            cx.dispatch_action(RunChunk);
            cx.run_until_parked();
            let (code, request) = harness.next_execution();
            harness.finish(&request, true, cx);
            code
        };
        // Lines 20 and 21 of the `many` chunk.
        assert_eq!(
            run_with_selection(Point::new(20, 0)..Point::new(21, 6), cx),
            "b <- 2\nc <- 3"
        );
        // A selection that starts on the fence takes only the code.
        assert_eq!(
            run_with_selection(Point::new(18, 0)..Point::new(19, 6), cx),
            "a <- 1"
        );
        // Selecting only the fence, or nothing, runs the whole chunk.
        assert_eq!(
            run_with_selection(Point::new(18, 0)..Point::new(18, 5), cx),
            "a <- 1\nb <- 2\nc <- 3"
        );
        assert_eq!(
            run_with_selection(Point::new(20, 2)..Point::new(20, 2), cx),
            "a <- 1\nb <- 2\nc <- 3"
        );
        // Each run's output replaces the last, under the same chunk.
        assert_eq!(
            harness
                .runner
                .read_with(cx, |runner, _| runner.outputs_for_test().len()),
            1
        );
    }

    #[gpui::test]
    async fn test_chunk_completions_come_from_ark(cx: &mut TestAppContext) {
        use futures::StreamExt as _;
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::sync::Mutex;

        // What Ark saw of the hidden chunk document, last first.
        let synced: Arc<Mutex<Vec<String>>> = Arc::default();
        let fake_servers = Rc::new(RefCell::new(None));
        let (harness, cx) = Harness::new_with(cx, {
            let synced = synced.clone();
            let fake_servers = fake_servers.clone();
            move |languages| {
                let servers = languages.register_fake_lsp(
                    "R Markdown",
                    language::FakeLspAdapter {
                        name: "ark",
                        capabilities: lsp::ServerCapabilities {
                            completion_provider: Some(lsp::CompletionOptions::default()),
                            ..Default::default()
                        },
                        initializer: Some(Box::new(move |server| {
                            let opened = synced.clone();
                            server
                                .handle_notification::<lsp::notification::DidOpenTextDocument, _>(
                                    move |params, _| {
                                        if params
                                            .text_document
                                            .uri
                                            .to_string()
                                            .contains("rezstudio-chunks")
                                        {
                                            opened
                                                .lock()
                                                .unwrap()
                                                .insert(0, params.text_document.text);
                                        }
                                    },
                                );
                            let changed = synced.clone();
                            server
                                .handle_notification::<lsp::notification::DidChangeTextDocument, _>(
                                    move |params, _| {
                                        if let Some(change) =
                                            params.content_changes.into_iter().last()
                                        {
                                            changed.lock().unwrap().insert(0, change.text);
                                        }
                                    },
                                );
                            server.set_request_handler::<HelpTopicRequest, _, _>(
                                |params, _| async move {
                                    let uri = params["textDocument"]["uri"].to_string();
                                    let position = &params["position"];
                                    let topic = (uri.contains("rezstudio-chunks")
                                        && position == &json!({ "line": 4, "character": 2 }))
                                        .then(|| json!({ "topic": "mtcars" }));
                                    Ok(topic)
                                },
                            );
                            server.set_request_handler::<lsp::request::Completion, _, _>(
                                |params, _| async move {
                                    let hidden = params
                                        .text_document_position
                                        .text_document
                                        .uri
                                        .to_string()
                                        .contains("rezstudio-chunks");
                                    // Asked about the .Rmd itself, Ark offers R names in prose.
                                    let label = if hidden { "mtcars" } else { "zznoise" };
                                    Ok(Some(lsp::CompletionResponse::Array(vec![
                                        lsp::CompletionItem {
                                            label: label.into(),
                                            ..Default::default()
                                        },
                                    ])))
                                },
                            );
                        })),
                        ..Default::default()
                    },
                );
                *fake_servers.borrow_mut() = Some(servers);
            }
        })
        .await;
        let mut servers = fake_servers.borrow_mut().take().unwrap();
        let _server = servers
            .next()
            .await
            .expect("Ark's server starts for R Markdown");
        cx.run_until_parked();

        let complete = |at: Point, typed: &str, cx: &mut VisualTestContext| {
            harness.editor.update_in(cx, |editor, window, cx| {
                editor.focus_handle(cx).focus(window, cx);
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([at..at])
                });
                editor.insert(typed, window, cx);
                editor.show_completions(&editor::actions::ShowCompletions, window, cx);
            });
            cx.run_until_parked();
        };

        // In a chunk: a new line under `x <- 1` (row 3).
        complete(Point::new(3, 6), "\nmtc", cx);
        assert!(
            harness
                .editor
                .read_with(cx, |editor, _| editor.has_visible_completions_menu())
        );
        harness
            .editor
            .update_in(cx, |editor, window, cx| {
                editor.confirm_completion(
                    &editor::actions::ConfirmCompletion::default(),
                    window,
                    cx,
                )
            })
            .unwrap()
            .await
            .unwrap();
        let line = harness.editor.update(cx, |editor, cx| {
            editor
                .text(cx)
                .lines()
                .nth(4)
                .unwrap_or_default()
                .to_string()
        });
        assert_eq!(line, "mtcars");
        // Ark saw only the chunks' code, each line in its place.
        let seen = synced.lock().unwrap().last().cloned().unwrap_or_default();
        let seen_lines: Vec<&str> = seen.split('\n').collect();
        assert_eq!(seen_lines.get(0), Some(&""), "prose is blank");
        assert_eq!(seen_lines.get(3), Some(&"x <- 1"));
        assert_eq!(seen_lines.get(4), Some(&"mtc"));
        assert_eq!(seen_lines.get(8), Some(&""), "Python chunks are blank too");

        // In prose, what Ark offers is left out.
        complete(Point::new(0, 8), " zz", cx);
        assert!(
            !harness
                .editor
                .read_with(cx, |editor, _| editor.has_visible_completions_menu())
        );

        // F1's topic comes from the hidden document too, and only in R code.
        let topic_at = |at: Point, cx: &mut VisualTestContext| {
            harness.editor.update_in(cx, |editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([at..at])
                });
            });
            cx.update(|_, cx| help_topic_at_cursor(&harness.editor, cx))
        };
        let topic = topic_at(Point::new(4, 2), cx).expect("the cursor is in a chunk");
        cx.run_until_parked();
        assert_eq!(topic.await.unwrap().as_deref(), Some("mtcars"));
        assert!(
            topic_at(Point::new(0, 2), cx).is_none(),
            "prose has no topic"
        );
    }

    enum HelpTopicRequest {}

    impl lsp::request::Request for HelpTopicRequest {
        type Params = serde_json::Value;
        type Result = Option<serde_json::Value>;
        const METHOD: &'static str = "positron/textDocument/helpTopic";
    }

    #[gpui::test]
    async fn test_run_lines_follow_edits(cx: &mut TestAppContext) {
        let (harness, cx) = Harness::new(cx).await;
        harness.editor.update_in(cx, |editor, window, cx| {
            editor.move_to_end(&Default::default(), window, cx);
            editor.insert("\n```{r more}\n1\n```\n", window, cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            harness
                .runner
                .read_with(cx, |runner, _| runner.lens_count()),
            5
        );

        // The shortcuts reach the runner through the workspace.
        cx.focus(&harness.editor);
        cx.dispatch_action(RunAllChunks);
        cx.run_until_parked();
        let mut harness = harness;
        let (code, _) = harness.next_execution();
        assert_eq!(code, "x <- 1");
    }
}
