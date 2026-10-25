//! Running an R Markdown or Quarto document's chunks in the project's R
//! session, one at a time, with each chunk's output in a block under it and
//! a "Run Chunk" line above it, like VS Code's and Positron's code lenses.

use crate::chunks::{Chunk, parse_chunks};
use crate::output::{ChunkOutput, RunState, aspect_from_options, drop_output};
use collections::{HashSet, VecDeque};
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock,
};
use editor::{Anchor, Editor, MultiBufferSnapshot, ToPoint as _};
use gpui::{AnyElement, App, Entity, MouseButton, Subscription, Task, WeakEntity, Window, px};
use language::Point;
use runtime::comms::PlotEvent;
use runtime::{RSession, SessionAdded, SessionEvent, SessionRegistry, SessionState};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;
use ui::{IconButton, IconName, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

/// Typing changes where chunks are; their run lines catch up once it pauses.
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(200);

struct OutputBlock {
    /// The start of the chunk's opening fence, which follows edits.
    header: Anchor,
    block_id: CustomBlockId,
    output: Entity<ChunkOutput>,
    request_id: Option<String>,
}

/// A chunk waiting to run, with the code to run when it's only part of it.
struct QueuedRun {
    header: Anchor,
    code: Option<String>,
}

struct LensBlock {
    header: Anchor,
    block_id: CustomBlockId,
}

pub struct ChunkRunner {
    editor: WeakEntity<Editor>,
    workspace: WeakEntity<Workspace>,
    session: Option<Entity<RSession>>,
    outputs: Vec<OutputBlock>,
    lenses: Vec<LensBlock>,
    /// Chunks waiting to run, oldest first.
    queue: VecDeque<QueuedRun>,
    /// The execution of the chunk running now.
    running: Option<String>,
    _rescan: Option<Task<()>>,
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl ChunkRunner {
    pub fn new(
        editor: &Entity<Editor>,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![cx.subscribe_in(
            &editor.read(cx).buffer().clone(),
            window,
            |this, _, event, _, cx| {
                if let multi_buffer::Event::Edited { .. } = event {
                    this.schedule_rescan(cx);
                }
            },
        )];
        let workspace_id = workspace.entity_id();
        if let Some(registry) = SessionRegistry::global(cx) {
            subscriptions.push(cx.subscribe_in(
                &registry,
                window,
                move |this, _, event: &SessionAdded, window, cx| {
                    if event.workspace_id == workspace_id {
                        this.set_session(event.session.clone(), window, cx);
                    }
                },
            ));
        }
        let mut this = Self {
            editor: editor.downgrade(),
            workspace: workspace.downgrade(),
            session: None,
            outputs: Vec::new(),
            lenses: Vec::new(),
            queue: VecDeque::new(),
            running: None,
            _rescan: None,
            _session_subscription: None,
            _subscriptions: subscriptions,
        };
        let existing = SessionRegistry::global(cx)
            .and_then(|registry| registry.read(cx).session(workspace_id).cloned());
        if let Some(session) = existing {
            this.set_session(session, window, cx);
        }
        this.rescan(cx);
        this
    }

    pub fn set_session(
        &mut self,
        session: Entity<RSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._session_subscription = Some(cx.subscribe_in(
            &session,
            window,
            |this, _, event, window, cx| this.handle_session_event(event, window, cx),
        ));
        self.session = Some(session);
    }

    fn snapshot(&self, cx: &App) -> Option<MultiBufferSnapshot> {
        let editor = self.editor.upgrade()?;
        Some(editor.read(cx).buffer().read(cx).snapshot(cx))
    }

    /// The chunks as the buffer is now.
    fn chunks(&self, cx: &App) -> Option<(MultiBufferSnapshot, Vec<Chunk>)> {
        let snapshot = self.snapshot(cx)?;
        let chunks = parse_chunks(&snapshot.text());
        Some((snapshot, chunks))
    }

    fn chunk_for_header(&self, header: Anchor, cx: &App) -> Option<Chunk> {
        let (snapshot, chunks) = self.chunks(cx)?;
        let row = header.to_point(&snapshot).row;
        chunks
            .into_iter()
            .find(|chunk| chunk.header_row == row && chunk.is_r())
    }

    pub fn run_chunk_at(&mut self, row: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some((snapshot, chunks)) = self.chunks(cx) else {
            return;
        };
        if let Some(chunk) = crate::chunks::chunk_at_row(&chunks, row).filter(|chunk| chunk.is_r())
        {
            let header = snapshot.anchor_before(Point::new(chunk.header_row, 0));
            self.enqueue(vec![(header, None)], window, cx);
        }
    }

    /// Runs the selected code when there is a selection in an R chunk, and
    /// the whole chunk at `row` otherwise. The output shows under the chunk
    /// either way.
    pub fn run_selection_or_chunk(
        &mut self,
        selection: Range<Point>,
        row: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((snapshot, chunks)) = self.chunks(cx) else {
            return;
        };
        let selected = (!selection.is_empty())
            .then(|| crate::chunks::chunk_at_row(&chunks, selection.start.row))
            .flatten()
            .filter(|chunk| chunk.is_r() && !chunk.code_rows.is_empty())
            .and_then(|chunk| {
                // Only the code counts: a selection that takes in a fence doesn't send
                // backticks to R.
                let last = chunk.code_rows.end - 1;
                let code_start = Point::new(chunk.code_rows.start, 0);
                let code_end =
                    Point::new(last, snapshot.line_len(multi_buffer::MultiBufferRow(last)));
                let start = selection.start.max(code_start);
                let end = selection.end.min(code_end);
                let code: String = snapshot.text_for_range(start..end).collect();
                (start < end && !code.trim().is_empty()).then(|| {
                    (
                        snapshot.anchor_before(Point::new(chunk.header_row, 0)),
                        code,
                    )
                })
            });
        match selected {
            Some((header, code)) => self.enqueue(vec![(header, Some(code))], window, cx),
            None => self.run_chunk_at(row, window, cx),
        }
    }

    /// Every R chunk before the one at `row` (or before `row`), skipping `eval=FALSE`.
    pub fn run_chunks_above(&mut self, row: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some((snapshot, chunks)) = self.chunks(cx) else {
            return;
        };
        let limit = crate::chunks::chunk_at_row(&chunks, row).map_or(row, |chunk| chunk.header_row);
        let headers = chunks
            .iter()
            .filter(|chunk| chunk.is_r() && !chunk.is_eval_false() && chunk.header_row < limit)
            .map(|chunk| {
                (
                    snapshot.anchor_before(Point::new(chunk.header_row, 0)),
                    None,
                )
            })
            .collect();
        self.enqueue(headers, window, cx);
    }

    pub fn run_all_chunks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((snapshot, chunks)) = self.chunks(cx) else {
            return;
        };
        let headers = chunks
            .iter()
            .filter(|chunk| chunk.is_r() && !chunk.is_eval_false())
            .map(|chunk| {
                (
                    snapshot.anchor_before(Point::new(chunk.header_row, 0)),
                    None,
                )
            })
            .collect();
        self.enqueue(headers, window, cx);
    }

    pub fn clear_outputs(&mut self, cx: &mut Context<Self>) {
        self.queue.clear();
        let blocks: Vec<OutputBlock> = std::mem::take(&mut self.outputs);
        let ids: HashSet<CustomBlockId> = blocks.iter().map(|block| block.block_id).collect();
        for block in &blocks {
            drop_output(&block.output, cx);
        }
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| editor.remove_blocks(ids, None, cx));
        }
    }

    fn remove_output(&mut self, block_id: CustomBlockId, cx: &mut Context<Self>) {
        let Some(index) = self
            .outputs
            .iter()
            .position(|block| block.block_id == block_id)
        else {
            return;
        };
        let block = self.outputs.remove(index);
        self.queue.retain(|queued| queued.header != block.header);
        drop_output(&block.output, cx);
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| {
                editor.remove_blocks(HashSet::from_iter([block_id]), None, cx)
            });
        }
    }

    fn enqueue(
        &mut self,
        runs: Vec<(Anchor, Option<String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.clone() else {
            return;
        };
        for (header, code) in runs {
            if self.queue.iter().any(|queued| queued.header == header) {
                continue;
            }
            let Some(chunk) = self.chunk_for_header(header, cx) else {
                continue;
            };
            let aspect = aspect_from_options(|key| chunk.option(key).map(str::to_string));
            let block = self.output_block(header, &chunk, &session, aspect, window, cx);
            let Some(block) = block else {
                continue;
            };
            if let Some(output) = self.outputs.get_mut(block) {
                // A rerun replaces what the chunk showed before.
                output.request_id = None;
                output
                    .output
                    .update(cx, |output, cx| output.reset(aspect, cx));
            }
            self.queue.push_back(QueuedRun { header, code });
        }
        self.run_next(cx);
    }

    /// The index of the chunk's output block, made if it has none yet.
    fn output_block(
        &mut self,
        header: Anchor,
        chunk: &Chunk,
        session: &Entity<RSession>,
        aspect: Option<f32>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if let Some(index) = self.outputs.iter().position(|block| block.header == header) {
            return Some(index);
        }
        let editor = self.editor.upgrade()?;
        let output =
            cx.new(|cx| ChunkOutput::new(session.clone(), self.workspace.clone(), aspect, cx));
        let runner = cx.entity().downgrade();
        let block_id = editor
            .update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let last_row = chunk.last_row();
                let end = Point::new(
                    last_row,
                    snapshot.line_len(multi_buffer::MultiBufferRow(last_row)),
                );
                let anchor = snapshot.anchor_before(end);
                editor.insert_blocks(
                    [BlockProperties {
                        placement: BlockPlacement::Below(anchor),
                        // At least a line for the status; the content sets the real height.
                        height: Some(1),
                        style: BlockStyle::Sticky,
                        render: output_renderer(output.clone(), runner),
                        priority: 0,
                    }],
                    None,
                    cx,
                )
            })
            .into_iter()
            .next()?;
        self.outputs.push(OutputBlock {
            header,
            block_id,
            output,
            request_id: None,
        });
        Some(self.outputs.len() - 1)
    }

    fn run_next(&mut self, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        while let Some(QueuedRun { header, code }) = self.queue.pop_front() {
            let Some(chunk) = self.chunk_for_header(header, cx) else {
                continue;
            };
            let Some(snapshot) = self.snapshot(cx) else {
                return;
            };
            let code = code.unwrap_or_else(|| chunk_code(&snapshot, &chunk));
            let Some(index) = self.outputs.iter().position(|block| block.header == header) else {
                continue;
            };
            match session.update(cx, |session, cx| session.execute_inline(code, cx)) {
                Ok(request_id) => {
                    self.running = Some(request_id.clone());
                    if let Some(block) = self.outputs.get_mut(index) {
                        block.request_id = Some(request_id);
                        block
                            .output
                            .update(cx, |output, cx| output.set_state(RunState::Running, cx));
                    }
                    return;
                }
                Err(error) => {
                    let reason = format!("Not run: {error:#}");
                    if let Some(block) = self.outputs.get(index) {
                        block.output.update(cx, |output, cx| {
                            output.set_state(RunState::Skipped(reason), cx)
                        });
                    }
                }
            }
        }
    }

    fn handle_session_event(
        &mut self,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::Output {
                request_id: Some(request_id),
                content,
            } => {
                if let Some(block) = self.block_for_request(request_id) {
                    block
                        .output
                        .update(cx, |output, cx| output.push(content, window, cx));
                }
            }
            SessionEvent::PlotOpened {
                comm_id,
                request_id: Some(request_id),
                ..
            } => {
                if let Some(block) = self.block_for_request(request_id) {
                    let comm_id = comm_id.clone();
                    block
                        .output
                        .update(cx, |output, cx| output.add_plot(comm_id, cx));
                }
            }
            SessionEvent::PlotEvent {
                comm_id,
                event: PlotEvent::Update { .. },
            } => {
                for block in &self.outputs {
                    if block.output.read(cx).plot_ids().any(|id| id == comm_id) {
                        block
                            .output
                            .update(cx, |output, cx| output.plot_changed(comm_id, cx));
                    }
                }
            }
            SessionEvent::ExecutionFinished {
                request_id,
                succeeded,
            } if self.running.as_deref() == Some(request_id.as_str()) => {
                self.running = None;
                if let Some(block) = self.block_for_request(request_id) {
                    let succeeded = *succeeded;
                    block.output.update(cx, |output, cx| {
                        output.set_state(RunState::Finished { succeeded }, cx)
                    });
                }
                if !*succeeded {
                    // As in RStudio, an error stops the chunks queued after it.
                    self.skip_queue("Not run: an earlier chunk failed.", cx);
                }
                self.run_next(cx);
            }
            SessionEvent::StateChanged => {
                let running = self.session.as_ref().is_some_and(|session| {
                    matches!(
                        session.read(cx).state(),
                        SessionState::Idle | SessionState::Busy
                    )
                });
                if !running {
                    self.running = None;
                    self.skip_queue("Not run: R isn't running.", cx);
                }
            }
            _ => {}
        }
    }

    fn skip_queue(&mut self, reason: &str, cx: &mut Context<Self>) {
        for queued in std::mem::take(&mut self.queue) {
            if let Some(block) = self
                .outputs
                .iter()
                .find(|block| block.header == queued.header)
            {
                let reason = reason.to_string();
                block.output.update(cx, |output, cx| {
                    output.set_state(RunState::Skipped(reason), cx)
                });
            }
        }
    }

    fn block_for_request(&self, request_id: &str) -> Option<&OutputBlock> {
        self.outputs
            .iter()
            .find(|block| block.request_id.as_deref() == Some(request_id))
    }

    fn schedule_rescan(&mut self, cx: &mut Context<Self>) {
        let timer = cx.background_executor().timer(RESCAN_DEBOUNCE);
        self._rescan = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| this.rescan(cx)).log_err();
        }));
    }

    /// Puts a run line above each R chunk, keeping the ones already in place.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let Some((snapshot, chunks)) = self.chunks(cx) else {
            return;
        };
        let wanted: Vec<u32> = chunks
            .iter()
            .filter(|chunk| chunk.is_r())
            .map(|chunk| chunk.header_row)
            .collect();
        let mut kept = Vec::new();
        let mut removed = HashSet::default();
        for lens in std::mem::take(&mut self.lenses) {
            let row = lens.header.to_point(&snapshot).row;
            if wanted.contains(&row) && !kept.iter().any(|(kept_row, _)| *kept_row == row) {
                kept.push((row, lens));
            } else {
                removed.insert(lens.block_id);
            }
        }
        let new_rows: Vec<u32> = wanted
            .into_iter()
            .filter(|row| !kept.iter().any(|(kept_row, _)| kept_row == row))
            .collect();
        let runner = cx.entity().downgrade();
        let new_lenses: Vec<(Anchor, BlockProperties<Anchor>)> = new_rows
            .iter()
            .map(|row| {
                let header = snapshot.anchor_before(Point::new(*row, 0));
                (
                    header,
                    BlockProperties {
                        placement: BlockPlacement::Above(header),
                        height: Some(1),
                        style: BlockStyle::Spacer,
                        render: lens_renderer(header, runner.clone()),
                        priority: 0,
                    },
                )
            })
            .collect();
        let (headers, properties): (Vec<Anchor>, Vec<BlockProperties<Anchor>>) =
            new_lenses.into_iter().unzip();
        let block_ids = editor.update(cx, |editor, cx| {
            if !removed.is_empty() {
                editor.remove_blocks(removed, None, cx);
            }
            editor.insert_blocks(properties, None, cx)
        });
        self.lenses = kept.into_iter().map(|(_, lens)| lens).collect();
        self.lenses.extend(
            headers
                .into_iter()
                .zip(block_ids)
                .map(|(header, block_id)| LensBlock { header, block_id }),
        );
    }

    #[cfg(test)]
    pub fn lens_count(&self) -> usize {
        self.lenses.len()
    }

    #[cfg(test)]
    pub fn outputs_for_test(&self) -> Vec<Entity<ChunkOutput>> {
        self.outputs
            .iter()
            .map(|block| block.output.clone())
            .collect()
    }
}

/// The text between a chunk's fences.
fn chunk_code(snapshot: &MultiBufferSnapshot, chunk: &Chunk) -> String {
    if chunk.code_rows.is_empty() {
        return String::new();
    }
    let start = Point::new(chunk.code_rows.start, 0);
    let last = chunk.code_rows.end - 1;
    let end = Point::new(last, snapshot.line_len(multi_buffer::MultiBufferRow(last)));
    snapshot.text_for_range(start..end).collect()
}

/// "▶ Run Chunk | Run Chunks Above" above a chunk, styled like a code lens.
fn lens_renderer(header: Anchor, runner: WeakEntity<ChunkRunner>) -> RenderBlock {
    Arc::new(move |cx: &mut BlockContext| {
        let text_style = &cx.editor_style.text;
        let font = text_style.font();
        let font_size = text_style.font_size.to_pixels(cx.window.rem_size()) * 0.9;
        let colors = cx.app.theme().colors();
        let color = colors
            .editor_code_lens_foreground
            .unwrap_or(colors.text_muted);
        let hover_color = colors.text;
        let item = |id: &'static str, title: &'static str, run_above: bool| {
            let runner = runner.clone();
            div()
                .id(id)
                .font(font.clone())
                .text_size(font_size)
                .text_color(color)
                .cursor_pointer()
                .hover(move |style| style.text_color(hover_color))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    let Some(runner) = runner.upgrade() else {
                        return;
                    };
                    runner.update(cx, |runner, cx| {
                        let Some(snapshot) = runner.snapshot(cx) else {
                            return;
                        };
                        let row = header.to_point(&snapshot).row;
                        if run_above {
                            runner.run_chunks_above(row, window, cx);
                        } else {
                            runner.run_chunk_at(row, window, cx);
                        }
                    });
                })
                .child(title)
        };
        div()
            .id(cx.block_id)
            // Spacer blocks start where the text does, like Zed's code lenses.
            .pl(cx.em_width * 0.5)
            .h_full()
            .flex()
            .flex_row()
            .items_end()
            .gap_1()
            .child(item("run-chunk", "▶ Run Chunk", false))
            .child(
                div()
                    .font(font.clone())
                    .text_size(font_size)
                    .text_color(color)
                    .child("|"),
            )
            .child(item("run-chunks-above", "Run Chunks Above", true))
            .into_any_element()
    })
}

/// The chunk's output, framed like Zed's REPL output, with a close button.
fn output_renderer(output: Entity<ChunkOutput>, runner: WeakEntity<ChunkRunner>) -> RenderBlock {
    Arc::new(move |cx: &mut BlockContext| -> AnyElement {
        let line_height = cx.line_height;
        let gutter = cx.margins.gutter.full_width();
        let block_id = cx.block_id;
        let runner = runner.clone();
        div()
            .id(cx.block_id)
            .block_mouse_except_scroll()
            .flex()
            .items_start()
            .min_h(line_height)
            .w_full()
            .border_y_1()
            .border_color(cx.app.theme().colors().border)
            .bg(cx.app.theme().colors().background)
            .child(
                div().relative().w(gutter).h(line_height * 2).child(
                    h_flex()
                        .absolute()
                        .top(line_height / 2.)
                        .right(gutter / 2. - line_height / 2.)
                        .child(
                            IconButton::new("close-chunk-output", IconName::Close)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Clear Output"))
                                .on_click(move |_, _, cx| {
                                    if let (
                                        Some(runner),
                                        editor::display_map::BlockId::Custom(block_id),
                                    ) = (runner.upgrade(), block_id)
                                    {
                                        runner.update(cx, |runner, cx| {
                                            runner.remove_output(block_id, cx)
                                        });
                                    }
                                }),
                        ),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .py(line_height / 2.)
                    .mr(cx.margins.right)
                    .pr(px(8.))
                    .child(output.clone()),
            )
            .into_any_element()
    })
}
