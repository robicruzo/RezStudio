//! RStudio's document outline: a list of an editor's sections, functions,
//! headings and chunks shown beside it, toggled per editor.

use collections::HashSet;
use editor::{Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};
use gpui::{
    App, Context, Entity, EntityId, Focusable, Global, ScrollHandle, Subscription, Task,
    WeakEntity, Window, actions, px,
};
use language::{BufferSnapshot, Point, ToPoint};
use r_markdown::chunks::{Chunk, parse_chunks};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{
    Pane, Workspace,
    item::{Item as _, ItemBufferKind},
};

actions!(
    outline_sidebar,
    [
        /// Shows or hides the outline beside the active editor.
        Toggle
    ]
);

const SIDEBAR_WIDTH: Pixels = px(220.);

/// The editors whose outline is shown.
#[derive(Default)]
struct OutlineToggles {
    editors: HashSet<EntityId>,
}

impl Global for OutlineToggles {}

pub fn init(cx: &mut App) {
    cx.set_global(OutlineToggles::default());
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, _, cx| {
            if let Some(editor) = workspace.active_item_as::<Editor>(cx) {
                toggle(&editor, cx);
            }
        });
    })
    .detach();
}

pub fn is_shown(editor: &Entity<Editor>, cx: &App) -> bool {
    cx.try_global::<OutlineToggles>()
        .is_some_and(|toggles| toggles.editors.contains(&editor.entity_id()))
}

pub fn toggle(editor: &Entity<Editor>, cx: &mut App) {
    if !cx.has_global::<OutlineToggles>() {
        cx.set_global(OutlineToggles::default());
    }
    let editor_id = editor.entity_id();
    let shown = cx.update_global::<OutlineToggles, _>(|toggles, _| {
        if toggles.editors.remove(&editor_id) {
            false
        } else {
            toggles.editors.insert(editor_id);
            true
        }
    });
    if shown {
        cx.observe_release(editor, move |_, cx| {
            if cx.has_global::<OutlineToggles>() {
                cx.update_global::<OutlineToggles, _>(|toggles, _| {
                    toggles.editors.remove(&editor_id);
                });
            }
        })
        .detach();
    }
}

/// Calls `callback` whenever an editor's outline is shown or hidden.
pub fn observe_toggles<T: 'static>(
    cx: &mut Context<T>,
    callback: impl Fn(&mut T, &mut Context<T>) + 'static,
) -> Subscription {
    cx.observe_global::<OutlineToggles>(callback)
}

/// Gives `pane` an outline sidebar, shown while its active editor's outline is on.
pub fn attach(pane: &Entity<Pane>, window: &mut Window, cx: &mut App) {
    let sidebar = cx.new(|cx| OutlineSidebar::new(pane, window, cx));
    pane.update(cx, |pane, cx| pane.set_side_view(Some(sidebar.into()), cx));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Section,
    Function,
    Heading,
    Chunk,
    Symbol,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineEntry {
    pub kind: EntryKind,
    pub label: String,
    pub depth: usize,
    pub row: u32,
}

/// An item from the language's outline query, reduced to what the sidebar needs.
#[derive(Clone, Debug)]
struct OutlineItem {
    depth: usize,
    text: String,
    row: u32,
}

pub struct OutlineSidebar {
    pane: WeakEntity<Pane>,
    editor: Option<Entity<Editor>>,
    entries: Vec<OutlineEntry>,
    current_entry: Option<usize>,
    scroll_handle: ScrollHandle,
    refresh_task: Task<()>,
    editor_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl OutlineSidebar {
    fn new(pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_in(pane, window, |this, _, _, cx| this.follow_active_editor(cx)),
            cx.observe_global::<OutlineToggles>(|this, cx| this.follow_active_editor(cx)),
        ];
        let mut this = Self {
            pane: pane.downgrade(),
            editor: None,
            entries: Vec::new(),
            current_entry: None,
            scroll_handle: ScrollHandle::new(),
            refresh_task: Task::ready(()),
            editor_subscription: None,
            _subscriptions: subscriptions,
        };
        this.follow_active_editor(cx);
        this
    }

    pub fn entries(&self) -> &[OutlineEntry] {
        &self.entries
    }

    pub fn current_entry(&self) -> Option<&OutlineEntry> {
        self.current_entry.and_then(|index| self.entries.get(index))
    }

    /// Shows the outline of the pane's active editor when its outline is on,
    /// and hides the sidebar otherwise.
    fn follow_active_editor(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.pane.upgrade() else {
            return;
        };
        let editor = pane
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<Editor>())
            .filter(|editor| {
                editor.read(cx).buffer_kind(cx) == ItemBufferKind::Singleton && is_shown(editor, cx)
            });
        let shown = editor.is_some();
        if editor.as_ref().map(Entity::entity_id) != self.editor.as_ref().map(Entity::entity_id) {
            self.editor_subscription = editor.as_ref().map(|editor| {
                cx.subscribe(editor, |this, _, event: &EditorEvent, cx| match event {
                    EditorEvent::Reparsed(_) | EditorEvent::BufferEdited => {
                        this.refresh_entries(cx)
                    }
                    EditorEvent::SelectionsChanged { .. } => this.update_current_entry(cx),
                    _ => {}
                })
            });
            self.editor = editor;
            self.entries.clear();
            self.current_entry = None;
            self.refresh_entries(cx);
            cx.notify();
        }
        if pane.read(cx).is_side_view_shown() != shown {
            pane.update(cx, |pane, cx| pane.set_side_view_shown(shown, cx));
        }
    }

    fn refresh_entries(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.editor.as_ref().and_then(|editor| {
            let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
            Some(buffer.read(cx).snapshot())
        }) else {
            self.refresh_task = Task::ready(());
            return;
        };
        let entries = cx.background_spawn(async move { entries_for_buffer(&snapshot) });
        self.refresh_task = cx.spawn(async move |this, cx| {
            let entries = entries.await;
            this.update(cx, |this, cx| {
                if this.entries != entries {
                    this.entries = entries;
                    this.current_entry = None;
                    cx.notify();
                }
                this.update_current_entry(cx);
            })
            .ok();
        });
    }

    /// Highlights the entry the cursor is in: the last one starting at or above it.
    fn update_current_entry(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let cursor_row = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head().row
        });
        let current_entry = self
            .entries
            .iter()
            .rposition(|entry| entry.row <= cursor_row);
        if current_entry != self.current_entry {
            self.current_entry = current_entry;
            if let Some(index) = current_entry {
                self.scroll_handle.scroll_to_item(index);
            }
            cx.notify();
        }
    }

    fn jump_to(&mut self, row: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        editor.update(cx, |editor, cx| {
            let point = Point::new(row, 0);
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()).unfold(),
                window,
                cx,
                |selections| selections.select_ranges([point..point]),
            );
            window.focus(&editor.focus_handle(cx), cx);
        });
    }
}

impl Render for OutlineSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let current_entry = self.current_entry;
        v_flex()
            .id("outline-sidebar")
            .debug_selector(|| "outline-sidebar".into())
            .w(SIDEBAR_WIDTH)
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .py_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .when(self.entries.is_empty(), |this| {
                this.child(
                    div().px_2().py_1().child(
                        Label::new("No outline")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .children(self.entries.iter().enumerate().map(|(index, entry)| {
                let row = entry.row;
                let (icon, label_color) = match entry.kind {
                    EntryKind::Section => (IconName::Hash, Color::Default),
                    EntryKind::Heading => (IconName::Hash, Color::Default),
                    EntryKind::Function => (IconName::Code, Color::Muted),
                    EntryKind::Chunk => (IconName::FileCode, Color::Muted),
                    EntryKind::Symbol => (IconName::Code, Color::Muted),
                };
                let item = ListItem::new(("outline-entry", index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(entry.depth)
                    .indent_step_size(px(12.))
                    .toggle_state(current_entry == Some(index))
                    .start_slot(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(
                        Label::new(entry.label.clone())
                            .size(LabelSize::Small)
                            .color(label_color)
                            .truncate(),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.jump_to(row, window, cx)),
                    );
                div()
                    .debug_selector(move || format!("outline-entry-{index}"))
                    .child(item)
            }))
    }
}

fn entries_for_buffer(snapshot: &BufferSnapshot) -> Vec<OutlineEntry> {
    let items: Vec<OutlineItem> = snapshot
        .outline(None)
        .items
        .into_iter()
        .map(|item| OutlineItem {
            depth: item.depth,
            text: item.text.to_string(),
            row: item.range.start.to_point(snapshot).row,
        })
        .collect();
    let language_name = snapshot.language().map(|language| language.name());
    match language_name.as_ref().map(|name| name.as_ref()) {
        Some("R") => r_entries(&items),
        Some("R Markdown" | "Quarto" | "Markdown") => {
            document_entries(&items, &parse_chunks(&snapshot.text()))
        }
        _ => items
            .into_iter()
            .map(|item| OutlineEntry {
                kind: EntryKind::Symbol,
                label: item.text,
                depth: item.depth,
                row: item.row,
            })
            .collect(),
    }
}

/// RStudio's R outline: code sections (`# Name ----`, deeper with `##`) and
/// the functions under them.
fn r_entries(items: &[OutlineItem]) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    let mut section_depth = None;
    for item in items {
        if item.text.starts_with('#') {
            let level = item.text.chars().take_while(|&c| c == '#').count();
            let label = item
                .text
                .trim_start_matches('#')
                .trim_end()
                .trim_end_matches(['-', '=', '#'])
                .trim();
            if label.is_empty() {
                continue;
            }
            let depth = level.saturating_sub(1);
            section_depth = Some(depth);
            entries.push(OutlineEntry {
                kind: EntryKind::Section,
                label: label.to_string(),
                depth,
                row: item.row,
            });
        } else {
            entries.push(OutlineEntry {
                kind: EntryKind::Function,
                label: item.text.trim_matches(['"', '\'', '`']).to_string(),
                depth: section_depth.map_or(0, |depth| depth + 1) + item.depth,
                row: item.row,
            });
        }
    }
    entries
}

/// R Markdown and Quarto: headings, with each chunk under the heading above it.
fn document_entries(items: &[OutlineItem], chunks: &[Chunk]) -> Vec<OutlineEntry> {
    // The outline also has items from embedded languages, such as the front
    // matter's YAML keys and functions in chunks, which RStudio leaves out.
    let mut headings = items
        .iter()
        .filter(|item| {
            item.text.starts_with('#') && !chunks.iter().any(|chunk| chunk.contains_row(item.row))
        })
        .peekable();
    let mut entries = Vec::new();
    let mut heading_depth = None;
    for (index, chunk) in chunks.iter().enumerate() {
        while let Some(heading) = headings.next_if(|heading| heading.row < chunk.header_row) {
            heading_depth = Some(heading.depth);
            entries.push(heading_entry(heading));
        }
        let number = index + 1;
        let label = match chunk.label.as_deref().or_else(|| chunk.option("label")) {
            Some(label) => format!("Chunk {number}: {label}"),
            None => format!("Chunk {number}"),
        };
        entries.push(OutlineEntry {
            kind: EntryKind::Chunk,
            label,
            depth: heading_depth.map_or(0, |depth| depth + 1),
            row: chunk.header_row,
        });
    }
    entries.extend(headings.map(heading_entry));
    entries
}

fn heading_entry(heading: &OutlineItem) -> OutlineEntry {
    OutlineEntry {
        kind: EntryKind::Heading,
        label: heading.text.trim_start_matches('#').trim().to_string(),
        depth: heading.depth,
        row: heading.row,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use indoc::indoc;
    use project::Project;
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    async fn test_sidebar_follows_the_editor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            serde_json::json!({
                "analysis.R": indoc! {"
                    # Load data ----
                    read_input <- function(path) {
                      read.csv(path)
                    }

                    # Model ----
                    fit <- function(data) lm(y ~ x, data)
                "},
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project
                .languages()
                .add(languages::language("r", tree_sitter_r::LANGUAGE.into()))
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = cx.read(|cx| multi_workspace.read(cx).workspace().clone());
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        cx.update(|window, cx| attach(&pane, window, cx));
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
                .expect("a worktree")
        });
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    (worktree_id, rel_path("analysis.R")),
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .expect("the file opens")
            .downcast::<Editor>()
            .expect("an editor");
        cx.run_until_parked();
        assert!(!pane.read_with(cx, |pane, _| pane.is_side_view_shown()));

        cx.dispatch_action(Toggle);
        cx.run_until_parked();
        assert!(pane.read_with(cx, |pane, _| pane.is_side_view_shown()));
        let sidebar = sidebar(&pane, cx);
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                summary(sidebar.entries()),
                vec![
                    (EntryKind::Section, "Load data", 0, 0),
                    (EntryKind::Function, "read_input", 1, 1),
                    (EntryKind::Section, "Model", 0, 5),
                    (EntryKind::Function, "fit", 1, 6),
                ]
            );
            assert_eq!(
                sidebar.current_entry().map(|entry| entry.label.as_str()),
                Some("Load data")
            );
        });

        let fit_entry = cx
            .debug_bounds("outline-entry-3")
            .expect("the fit entry is drawn");
        cx.simulate_click(fit_entry.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar.current_entry().map(|entry| entry.label.as_str()),
                Some("fit")
            );
        });
        editor.update_in(cx, |editor, window, cx| {
            assert!(editor.focus_handle(cx).is_focused(window));
        });

        cx.dispatch_action(Toggle);
        cx.run_until_parked();
        assert!(!pane.read_with(cx, |pane, _| pane.is_side_view_shown()));
    }

    fn sidebar(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Entity<OutlineSidebar> {
        pane.read_with(cx, |pane, _| pane.side_view())
            .and_then(|view| view.downcast::<OutlineSidebar>().ok())
            .expect("the pane has an outline sidebar")
    }

    fn item(depth: usize, text: &str, row: u32) -> OutlineItem {
        OutlineItem {
            depth,
            text: text.to_string(),
            row,
        }
    }

    fn summary(entries: &[OutlineEntry]) -> Vec<(EntryKind, &str, usize, u32)> {
        entries
            .iter()
            .map(|entry| (entry.kind, entry.label.as_str(), entry.depth, entry.row))
            .collect()
    }

    #[test]
    fn test_r_entries_nest_functions_under_sections() {
        let entries = r_entries(&[
            item(0, "helper", 0),
            item(0, "# Load data ----", 2),
            item(0, "read_input", 3),
            item(1, "inner", 4),
            item(0, "## Clean ====", 8),
            item(0, "\"tidy\"", 9),
            item(0, "# ----", 12),
            item(0, "# Model ####", 14),
        ]);
        assert_eq!(
            summary(&entries),
            vec![
                (EntryKind::Function, "helper", 0, 0),
                (EntryKind::Section, "Load data", 0, 2),
                (EntryKind::Function, "read_input", 1, 3),
                (EntryKind::Function, "inner", 2, 4),
                (EntryKind::Section, "Clean", 1, 8),
                (EntryKind::Function, "tidy", 2, 9),
                (EntryKind::Section, "Model", 0, 14),
            ]
        );
    }

    #[test]
    fn test_document_entries_put_chunks_under_headings() {
        let chunks = parse_chunks(
            "```{r setup}\n1\n```\n# Intro\n```{r}\n2\n```\n## Detail\n```{python}\n#| label: py\n3\n```\n",
        );
        let entries = document_entries(
            &[
                item(0, "title", 0),
                item(0, "# Intro", 3),
                item(1, "# not a heading ----", 5),
                item(1, "## Detail", 7),
            ],
            &chunks,
        );
        assert_eq!(
            summary(&entries),
            vec![
                (EntryKind::Chunk, "Chunk 1: setup", 0, 0),
                (EntryKind::Heading, "Intro", 0, 3),
                (EntryKind::Chunk, "Chunk 2", 1, 4),
                (EntryKind::Heading, "Detail", 1, 7),
                (EntryKind::Chunk, "Chunk 3: py", 2, 8),
            ]
        );
    }
}
