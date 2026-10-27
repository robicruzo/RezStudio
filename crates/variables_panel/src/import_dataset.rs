//! RStudio's Import Dataset dialog: pick a file, preview its first rows with
//! the chosen options, and run the generated code in the console.

use crate::{EnvironmentPanel, display_path};
use anyhow::{Context as _, Result, bail};
use editor::Editor;
use gpui::{
    Anchor, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, Subscription, Task, WeakEntity, Window,
};
use runtime::{RSession, r_string};
use serde_json::Value;
use std::time::Duration;
use ui::{Checkbox, ContextMenu, IconButton, PopoverMenu, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

/// Rows read for the preview, as in RStudio.
const PREVIEW_ROWS: u32 = 50;
/// How long typing pauses before the preview is read again.
const PREVIEW_DELAY: Duration = Duration::from_millis(300);
const PREVIEW_COLUMN_WIDTH: Pixels = px(140.);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Text,
    Excel,
    Spss,
    Sas,
    Stata,
}

impl Source {
    pub const ALL: [Source; 5] = [
        Source::Text,
        Source::Excel,
        Source::Spss,
        Source::Sas,
        Source::Stata,
    ];

    pub fn menu_label(self) -> &'static str {
        match self {
            Source::Text => "From Text (readr)…",
            Source::Excel => "From Excel…",
            Source::Spss => "From SPSS…",
            Source::Sas => "From SAS…",
            Source::Stata => "From Stata…",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Source::Text => "Import Text Data",
            Source::Excel => "Import Excel Data",
            Source::Spss => "Import SPSS Data",
            Source::Sas => "Import SAS Data",
            Source::Stata => "Import Stata Data",
        }
    }

    fn package(self) -> &'static str {
        match self {
            Source::Text => "readr",
            Source::Excel => "readxl",
            Source::Spss | Source::Sas | Source::Stata => "haven",
        }
    }

    /// Whether the reader takes the header, skip and missing value options.
    fn has_reading_options(self) -> bool {
        matches!(self, Source::Text | Source::Excel)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delimiter {
    #[default]
    Comma,
    Tab,
    Semicolon,
    Whitespace,
}

impl Delimiter {
    const ALL: [Delimiter; 4] = [
        Delimiter::Comma,
        Delimiter::Tab,
        Delimiter::Semicolon,
        Delimiter::Whitespace,
    ];

    fn label(self) -> &'static str {
        match self {
            Delimiter::Comma => "Comma",
            Delimiter::Tab => "Tab",
            Delimiter::Semicolon => "Semicolon",
            Delimiter::Whitespace => "Whitespace",
        }
    }
}

/// What the dialog's fields say, from which both the preview and the
/// imported code are built.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportOptions {
    pub source: Source,
    /// As written in R code: `~/data.csv`, a full path, or a URL.
    pub path: String,
    pub name: String,
    pub delimiter: Delimiter,
    pub first_row_as_names: bool,
    pub skip: u32,
    /// The text read as missing; empty keeps the reader's default.
    pub na: String,
    /// The Excel sheet; `None` reads the first.
    pub sheet: Option<String>,
    pub open_viewer: bool,
}

impl ImportOptions {
    /// The reader's call, with only the arguments that differ from its
    /// defaults, as RStudio writes it.
    fn reader_call(&self, namespace: &str, extra: &[&str]) -> String {
        let mut arguments = vec![r_string(&self.path)];
        let function = match self.source {
            Source::Text => match self.delimiter {
                Delimiter::Comma => "read_csv",
                Delimiter::Tab => "read_tsv",
                Delimiter::Semicolon => {
                    arguments.push("delim = \";\"".to_string());
                    "read_delim"
                }
                Delimiter::Whitespace => "read_table",
            },
            Source::Excel => {
                if let Some(sheet) = &self.sheet {
                    arguments.push(format!("sheet = {}", r_string(sheet)));
                }
                "read_excel"
            }
            Source::Spss => "read_sav",
            Source::Sas => "read_sas",
            Source::Stata => "read_dta",
        };
        if self.source.has_reading_options() {
            if !self.first_row_as_names {
                arguments.push("col_names = FALSE".to_string());
            }
            if self.skip > 0 {
                arguments.push(format!("skip = {}", self.skip));
            }
            if !self.na.is_empty() {
                arguments.push(format!("na = {}", r_string(&self.na)));
            }
        }
        arguments.extend(extra.iter().map(|argument| argument.to_string()));
        format!("{namespace}{function}({})", arguments.join(", "))
    }

    /// The code Import runs in the console.
    pub fn import_code(&self) -> String {
        let name = r_name(&self.name);
        let mut lines = vec![
            format!("library({})", self.source.package()),
            format!("{name} <- {}", self.reader_call("", &[])),
        ];
        if self.open_viewer {
            lines.push(format!("View({name})"));
        }
        lines.join("\n")
    }

    /// R code reading the first rows with these options, returning each
    /// column's name, class and values formatted as text, and an Excel
    /// file's sheets. It's run silently and leaves nothing behind.
    pub fn preview_code(&self) -> String {
        let package = self.source.package();
        let rows = format!("n_max = {PREVIEW_ROWS}");
        let mut extra = vec![rows.as_str()];
        if self.source == Source::Text {
            extra.push("show_col_types = FALSE");
            extra.push("progress = FALSE");
        }
        let sheets = if self.source == Source::Excel {
            format!("readxl::excel_sheets({})", r_string(&self.path))
        } else {
            "NULL".to_string()
        };
        format!(
            "local({{
    data <- {reader}
    list(
        names = names(data),
        types = unname(vapply(data, function(column) class(column)[1], \"\")),
        columns = unname(lapply(data, function(column) trimws(format(column)))),
        rows = nrow(data),
        sheets = {sheets}
    )
}})",
            reader = self.reader_call(&format!("{package}::"), &extra),
        )
    }
}

/// A name as R code: plain when R accepts it as is, in backticks otherwise.
fn r_name(name: &str) -> String {
    const RESERVED: [&str; 14] = [
        "if", "else", "repeat", "while", "function", "for", "next", "break", "TRUE", "FALSE",
        "NULL", "Inf", "NaN", "NA",
    ];
    let mut characters = name.chars();
    let starts_well = match characters.next() {
        Some(first) if first.is_ascii_alphabetic() => true,
        Some('.') => !characters
            .next()
            .is_some_and(|second| second.is_ascii_digit()),
        _ => false,
    };
    let syntactic = starts_well
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_'))
        && !RESERVED.contains(&name);
    if syntactic {
        name.to_string()
    } else {
        format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
    }
}

/// The name RStudio suggests for a file: its name without the extension,
/// with characters R doesn't allow in names replaced by `_`.
pub fn default_name(path: &str) -> String {
    let file_name = path
        .trim_end_matches('/')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let stem = file_name
        .split_once('.')
        .map_or(file_name, |(stem, _)| stem);
    let name: String = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    match name.chars().next() {
        None => "dataset".to_string(),
        Some(first) if first.is_ascii_alphabetic() => name,
        Some(_) => format!("X{name}"),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewColumn {
    pub name: String,
    pub type_name: String,
    pub cells: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub columns: Vec<PreviewColumn>,
    pub rows: usize,
    pub sheets: Vec<String>,
}

/// Reads [`ImportOptions::preview_code`]'s result. R's vectors arrive as a
/// string when they hold one value and as null when empty.
pub fn parse_preview(result: Value) -> Result<Preview> {
    let Value::Object(mut fields) = result else {
        bail!("unexpected preview: {result}");
    };
    let mut take = |key: &str| fields.remove(key).unwrap_or(Value::Null);
    let names = strings(take("names"))?;
    let types = strings(take("types"))?;
    let columns = match take("columns") {
        Value::Null => Vec::new(),
        Value::Array(columns) => columns
            .into_iter()
            .map(strings)
            .collect::<Result<Vec<_>>>()?,
        other => bail!("unexpected preview columns: {other}"),
    };
    let rows = take("rows")
        .as_u64()
        .and_then(|rows| usize::try_from(rows).ok())
        .context("the preview has no row count")?;
    let sheets = strings(take("sheets"))?;
    if names.len() != columns.len() {
        bail!(
            "the preview has {} names for {} columns",
            names.len(),
            columns.len()
        );
    }
    let columns = names
        .into_iter()
        .zip(columns)
        .enumerate()
        .map(|(index, (name, cells))| PreviewColumn {
            name,
            type_name: types.get(index).cloned().unwrap_or_default(),
            cells,
        })
        .collect();
    Ok(Preview {
        columns,
        rows,
        sheets,
    })
}

fn strings(value: Value) -> Result<Vec<String>> {
    let text = |value: Value| match value {
        Value::String(text) => Ok(text),
        Value::Null => Ok("NA".to_string()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(value) => Ok(if value { "TRUE" } else { "FALSE" }.to_string()),
        other => bail!("unexpected value in the preview: {other}"),
    };
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(values) => values.into_iter().map(text).collect(),
        value => Ok(vec![text(value)?]),
    }
}

enum PreviewState {
    NoFile,
    Loading,
    Ready(Preview),
    Failed(String),
}

pub struct ImportDataset {
    panel: WeakEntity<EnvironmentPanel>,
    session: Entity<RSession>,
    source: Source,
    path: Entity<Editor>,
    name: Entity<Editor>,
    skip: Entity<Editor>,
    na: Entity<Editor>,
    /// The code Import runs, read-only so it can be selected and copied.
    code: Entity<Editor>,
    /// What `code` shows, to update it only when the options change.
    shown_code: String,
    delimiter: Delimiter,
    first_row_as_names: bool,
    open_viewer: bool,
    sheet: Option<String>,
    /// Whether the name was typed, after which a new file keeps it.
    name_edited: bool,
    /// Set while the dialog fills in the name itself.
    filling_name: bool,
    preview: PreviewState,
    preview_task: Task<()>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ImportDataset {
    pub fn new(
        source: Source,
        panel: WeakEntity<EnvironmentPanel>,
        session: Entity<RSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_string();
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(&placeholder, window, cx);
                editor
            })
        };
        let path = editor("Path or URL", window, cx);
        let name = editor("Name", window, cx);
        let skip = editor("0", window, cx);
        let na = editor("Default", window, cx);
        let code = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 8, window, cx);
            editor.set_read_only(true);
            editor.set_show_gutter(false, cx);
            editor
        });
        let is_edit =
            |event: &editor::EditorEvent| matches!(event, editor::EditorEvent::BufferEdited);
        let subscriptions = vec![
            cx.subscribe_in(&path, window, move |this, _, event, window, cx| {
                if is_edit(event) {
                    this.path_changed(window, cx);
                }
            }),
            cx.subscribe_in(&name, window, move |this, _, event, _, cx| {
                if is_edit(event) {
                    if !this.filling_name {
                        this.name_edited = true;
                    }
                    cx.notify();
                }
            }),
            cx.subscribe_in(&skip, window, move |this, _, event, _, cx| {
                if is_edit(event) {
                    this.schedule_preview(cx);
                }
            }),
            cx.subscribe_in(&na, window, move |this, _, event, _, cx| {
                if is_edit(event) {
                    this.schedule_preview(cx);
                }
            }),
        ];
        let focus_handle = path.focus_handle(cx);
        let mut this = Self {
            panel,
            session,
            source,
            path,
            name,
            skip,
            na,
            code,
            shown_code: String::new(),
            delimiter: Delimiter::default(),
            first_row_as_names: true,
            open_viewer: true,
            sheet: None,
            name_edited: false,
            filling_name: false,
            preview: PreviewState::NoFile,
            preview_task: Task::ready(()),
            focus_handle,
            _subscriptions: subscriptions,
        };
        this.browse(window, cx);
        this
    }

    fn options(&self, cx: &App) -> ImportOptions {
        ImportOptions {
            source: self.source,
            path: self.path.read(cx).text(cx).trim().to_string(),
            name: self.name.read(cx).text(cx).trim().to_string(),
            delimiter: self.delimiter,
            first_row_as_names: self.first_row_as_names,
            skip: self.skip.read(cx).text(cx).trim().parse().unwrap_or(0),
            na: self.na.read(cx).text(cx),
            sheet: self.sheet.clone(),
            open_viewer: self.open_viewer,
        }
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.first() else {
                return;
            };
            let path = display_path(path);
            this.update_in(cx, |this, window, cx| {
                this.path
                    .update(cx, |editor, cx| editor.set_text(path, window, cx))
            })
            .log_err();
        })
        .detach();
    }

    fn path_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Another file's sheets differ.
        self.sheet = None;
        if !self.name_edited {
            let name = default_name(self.path.read(cx).text(cx).trim());
            self.filling_name = true;
            self.name
                .update(cx, |editor, cx| editor.set_text(name, window, cx));
            self.filling_name = false;
        }
        self.schedule_preview(cx);
    }

    fn schedule_preview(&mut self, cx: &mut Context<Self>) {
        if self.options(cx).path.is_empty() {
            self.preview = PreviewState::NoFile;
            self.preview_task = Task::ready(());
            cx.notify();
            return;
        }
        self.preview = PreviewState::Loading;
        cx.notify();
        // Replacing the task drops a preview still waiting, so only the last
        // options' preview is shown.
        self.preview_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_DELAY).await;
            let Ok(reply) = this.update(cx, |this, cx| {
                let code = this.options(cx).preview_code();
                this.session
                    .update(cx, |session, cx| session.evaluate_code(&code, cx))
            }) else {
                return;
            };
            let preview = reply.await.and_then(|reply| parse_preview(reply.result));
            this.update(cx, |this, cx| {
                this.preview = match preview {
                    Ok(preview) => PreviewState::Ready(preview),
                    Err(error) => PreviewState::Failed(format!("{error:#}")),
                };
                cx.notify();
            })
            .log_err();
        });
    }

    pub(crate) fn set_delimiter(&mut self, delimiter: Delimiter, cx: &mut Context<Self>) {
        if self.delimiter != delimiter {
            self.delimiter = delimiter;
            self.schedule_preview(cx);
        }
    }

    fn set_sheet(&mut self, sheet: Option<String>, cx: &mut Context<Self>) {
        if self.sheet != sheet {
            self.sheet = sheet;
            self.schedule_preview(cx);
        }
    }

    fn can_import(&self, cx: &App) -> bool {
        let options = self.options(cx);
        !options.path.is_empty() && !options.name.is_empty()
    }

    pub(crate) fn import(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_import(cx) {
            return;
        }
        let code = self.options(cx).import_code();
        self.panel
            .update(cx, |panel, cx| panel.run_in_console(code, window, cx))
            .log_err();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    #[cfg(test)]
    pub(crate) fn code_text(&self, cx: &App) -> String {
        self.code.read(cx).text(cx)
    }

    fn render_field(
        &self,
        label: &'static str,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .px_1p5()
                    .py_0p5()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .child(editor.clone()),
            )
    }

    fn render_choice(
        &self,
        id: &'static str,
        label: &'static str,
        current: String,
        menu: impl Fn(&mut Window, &mut App) -> Option<Entity<ContextMenu>> + 'static,
    ) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                PopoverMenu::new(id)
                    .trigger(
                        Button::new(SharedString::from(format!("{id}-trigger")), current)
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .end_icon(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .anchor(Anchor::TopLeft)
                    .menu(menu),
            )
    }

    fn render_preview(&self, cx: &App) -> AnyElement {
        let message = |text: String, color: Color| {
            div()
                .p_4()
                .child(Label::new(text).color(color))
                .into_any_element()
        };
        let preview = match &self.preview {
            PreviewState::NoFile => {
                return message("Choose a file to preview it.".to_string(), Color::Muted);
            }
            PreviewState::Loading => return message("Reading…".to_string(), Color::Muted),
            PreviewState::Failed(error) => return message(error.clone(), Color::Error),
            PreviewState::Ready(preview) => preview,
        };
        if preview.columns.is_empty() {
            return message("The file has no columns.".to_string(), Color::Muted);
        }
        let border = cx.theme().colors().border_variant;
        let cell = |text: String, color: Color| {
            div()
                .w(PREVIEW_COLUMN_WIDTH)
                .flex_none()
                .px_1p5()
                .py_0p5()
                .border_r_1()
                .border_color(border)
                .overflow_hidden()
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(color)
                        .truncate(),
                )
        };
        let header = h_flex()
            .border_b_1()
            .border_color(border)
            .bg(cx.theme().colors().surface_background)
            .children(preview.columns.iter().map(|column| {
                div()
                    .w(PREVIEW_COLUMN_WIDTH)
                    .flex_none()
                    .px_1p5()
                    .py_0p5()
                    .border_r_1()
                    .border_color(border)
                    .child(
                        Label::new(column.name.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .truncate(),
                    )
                    .child(
                        Label::new(format!("({})", column.type_name))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
            }));
        let rows = (0..preview.rows).map(|row| {
            h_flex().children(preview.columns.iter().map(|column| {
                let text = column.cells.get(row).cloned().unwrap_or_default();
                let color = if text == "NA" {
                    Color::Muted
                } else {
                    Color::Default
                };
                cell(text, color)
            }))
        });
        div()
            .id("import-preview-table")
            .size_full()
            .overflow_scroll()
            .child(v_flex().child(header).children(rows))
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for ImportDataset {}

impl Focusable for ImportDataset {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for ImportDataset {}

impl Render for ImportDataset {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let options = self.options(cx);
        let this = cx.weak_entity();
        let reading_options = self.source.has_reading_options();
        let sheets = match &self.preview {
            PreviewState::Ready(preview) => preview.sheets.clone(),
            _ => Vec::new(),
        };

        let mut settings = h_flex().gap_3().items_start().child(
            v_flex()
                .w(px(180.))
                .gap_2()
                .child(self.render_field("Name", &self.name, cx))
                .when(reading_options, |column| {
                    column
                        .child(self.render_field("Skip", &self.skip, cx))
                        .child(self.render_field("Missing Values", &self.na, cx))
                }),
        );
        let mut choices = v_flex().gap_2();
        if self.source == Source::Text {
            let this = this.clone();
            choices = choices.child(self.render_choice(
                "delimiter",
                "Delimiter",
                self.delimiter.label().to_string(),
                move |window, cx| {
                    let this = this.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for delimiter in Delimiter::ALL {
                            let this = this.clone();
                            menu = menu.entry(delimiter.label(), None, move |_, cx| {
                                this.update(cx, |this, cx| this.set_delimiter(delimiter, cx))
                                    .log_err();
                            });
                        }
                        menu
                    }))
                },
            ));
        }
        if self.source == Source::Excel {
            choices = choices.child(self.render_choice(
                "sheet",
                "Sheet",
                self.sheet.clone().unwrap_or_else(|| "Default".to_string()),
                move |window, cx| {
                    let this = this.clone();
                    let sheets = sheets.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        let choices = std::iter::once(None).chain(sheets.iter().cloned().map(Some));
                        for sheet in choices {
                            let this = this.clone();
                            let label = sheet.clone().unwrap_or_else(|| "Default".to_string());
                            menu = menu.entry(label, None, move |_, cx| {
                                this.update(cx, |this, cx| this.set_sheet(sheet.clone(), cx))
                                    .log_err();
                            });
                        }
                        menu
                    }))
                },
            ));
        }
        if reading_options {
            choices = choices.child(
                Checkbox::new(
                    "first-row-as-names",
                    ToggleState::from(self.first_row_as_names),
                )
                .label("First Row as Names")
                .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                    this.first_row_as_names = state.selected();
                    this.schedule_preview(cx);
                })),
            );
        }
        choices = choices.child(
            Checkbox::new("open-data-viewer", ToggleState::from(self.open_viewer))
                .label("Open Data Viewer")
                .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                    this.open_viewer = state.selected();
                    cx.notify();
                })),
        );
        settings = settings.child(choices);

        let code = if options.path.is_empty() || options.name.is_empty() {
            String::new()
        } else {
            options.import_code()
        };
        if code != self.shown_code {
            self.code
                .update(cx, |editor, cx| editor.set_text(code.clone(), window, cx));
            self.shown_code = code.clone();
        }
        let can_import = self.can_import(cx);

        v_flex()
            .key_context("ImportDataset")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::import))
            .w(px(760.))
            .elevation_3(cx)
            .p_3()
            .gap_3()
            .child(Headline::new(self.source.title()).size(HeadlineSize::Small))
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.render_field("File", &self.path, cx)),
                    )
                    .child(
                        Button::new("browse", "Browse…")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                    ),
            )
            .child(
                div()
                    .h(px(260.))
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .child(self.render_preview(cx)),
            )
            .child(
                h_flex().gap_3().items_start().child(settings).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .justify_between()
                                .child(
                                    Label::new("Code Preview")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    IconButton::new("copy-import-code", IconName::Copy)
                                        .icon_size(IconSize::Small)
                                        .disabled(code.is_empty())
                                        .tooltip(Tooltip::text("Copy Code"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                code.clone(),
                                            ))
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .p_1p5()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .rounded_sm()
                                .bg(cx.theme().colors().editor_background)
                                .child(self.code.clone()),
                        ),
                ),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("cancel-import", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("import", "Import")
                            .style(ButtonStyle::Filled)
                            .disabled(!can_import)
                            .tooltip(Tooltip::text("Run the code in the console"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.import(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options(source: Source, path: &str, name: &str) -> ImportOptions {
        ImportOptions {
            source,
            path: path.to_string(),
            name: name.to_string(),
            delimiter: Delimiter::default(),
            first_row_as_names: true,
            skip: 0,
            na: String::new(),
            sheet: None,
            open_viewer: true,
        }
    }

    #[test]
    fn test_import_code_matches_rstudio() {
        assert_eq!(
            options(Source::Text, "~/data/cars.csv", "cars").import_code(),
            "library(readr)\ncars <- read_csv(\"~/data/cars.csv\")\nView(cars)"
        );
        let mut text = options(Source::Text, "/tmp/a b.txt", "a_b");
        text.delimiter = Delimiter::Semicolon;
        text.first_row_as_names = false;
        text.skip = 2;
        text.na = "-".to_string();
        text.open_viewer = false;
        assert_eq!(
            text.import_code(),
            "library(readr)\na_b <- read_delim(\"/tmp/a b.txt\", delim = \";\", col_names = FALSE, skip = 2, na = \"-\")"
        );
        text.delimiter = Delimiter::Tab;
        assert!(
            text.import_code()
                .contains("read_tsv(\"/tmp/a b.txt\", col_names")
        );
        text.delimiter = Delimiter::Whitespace;
        assert!(text.import_code().contains("read_table("));

        let mut excel = options(Source::Excel, "~/book.xlsx", "book");
        excel.sheet = Some("Sales 2024".to_string());
        assert_eq!(
            excel.import_code(),
            "library(readxl)\nbook <- read_excel(\"~/book.xlsx\", sheet = \"Sales 2024\")\nView(book)"
        );

        // The statistical formats take no reading options.
        let mut stata = options(Source::Stata, "~/survey.dta", "survey");
        stata.skip = 3;
        assert_eq!(
            stata.import_code(),
            "library(haven)\nsurvey <- read_dta(\"~/survey.dta\")\nView(survey)"
        );
        assert!(
            options(Source::Spss, "x.sav", "x")
                .import_code()
                .contains("read_sav(")
        );
        assert!(
            options(Source::Sas, "x.sas7bdat", "x")
                .import_code()
                .contains("read_sas(")
        );
    }

    #[test]
    fn test_preview_code_reads_the_first_rows_quietly() {
        let code = options(Source::Text, "~/cars.csv", "cars").preview_code();
        assert!(
            code.contains(
                "readr::read_csv(\"~/cars.csv\", n_max = 50, show_col_types = FALSE, progress = FALSE)"
            ),
            "{code}"
        );
        assert!(code.contains("sheets = NULL"));
        assert!(!code.contains("View("));

        let code = options(Source::Excel, "~/book.xlsx", "book").preview_code();
        assert!(code.contains("readxl::read_excel(\"~/book.xlsx\", n_max = 50)"));
        assert!(code.contains("sheets = readxl::excel_sheets(\"~/book.xlsx\")"));
    }

    #[test]
    fn test_names() {
        assert_eq!(default_name("~/data/cars.csv"), "cars");
        assert_eq!(default_name("/tmp/my-data 2.tar.gz"), "my_data_2");
        assert_eq!(default_name("2024.csv"), "X2024");
        assert_eq!(default_name("https://example.com/files/"), "files");
        assert_eq!(default_name(""), "dataset");

        assert_eq!(r_name("cars"), "cars");
        assert_eq!(r_name(".hidden"), ".hidden");
        assert_eq!(r_name("my data"), "`my data`");
        assert_eq!(r_name("2x"), "`2x`");
        assert_eq!(r_name(".2x"), "`.2x`");
        assert_eq!(r_name("if"), "`if`");
    }

    #[test]
    fn test_parse_preview() {
        let preview = parse_preview(json!({
            "names": ["speed", "dist"],
            "types": ["numeric", "character"],
            "columns": [["4", "7"], ["2", "NA"]],
            "rows": 2,
            "sheets": null
        }))
        .unwrap();
        assert_eq!(preview.rows, 2);
        assert_eq!(preview.columns[1].name, "dist");
        assert_eq!(preview.columns[1].type_name, "character");
        assert_eq!(preview.columns[1].cells, ["2", "NA"]);
        assert!(preview.sheets.is_empty());

        // One column of one row, and one sheet, arrive as single strings.
        let preview = parse_preview(json!({
            "names": "x", "types": "numeric", "columns": ["1"], "rows": 1, "sheets": "Sheet1"
        }))
        .unwrap();
        assert_eq!(preview.columns[0].cells, ["1"]);
        assert_eq!(preview.sheets, ["Sheet1"]);

        // No rows.
        let preview = parse_preview(json!({
            "names": "x", "types": "logical", "columns": [null], "rows": 0, "sheets": null
        }))
        .unwrap();
        assert!(preview.columns[0].cells.is_empty());

        assert!(
            parse_preview(json!({ "names": ["a", "b"], "columns": [["1"]], "rows": 1 })).is_err()
        );
        assert!(parse_preview(json!("oops")).is_err());
    }
}
