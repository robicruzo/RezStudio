use crate::focus_follows_mouse::FocusFollowsMouse as _;
use crate::persistence::model::DockData;
use crate::status_bar::HideStatusItem;
use crate::{DraggedDock, Event, FocusFollowsMouse, ModalLayer, Pane, WorkspaceSettings};
use crate::{Workspace, status_bar::StatusItemView};
use anyhow::Context as _;
use client::proto;
use db::kvp::KeyValueStore;

use gpui::{
    Action, Anchor, AnyView, App, Axis, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, IntoElement, KeyContext, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement,
    Render, SharedString, StyleRefinement, Styled, Subscription, WeakEntity, Window, deferred, div,
    px,
};
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore, TerminalDockPosition};
use std::sync::Arc;
use ui::{
    ContextMenu, CountBadge, Divider, DividerColor, IconButton, Tooltip, prelude::*,
    right_click_menu,
};
use util::ResultExt as _;

pub(crate) const RESIZE_HANDLE_SIZE: Pixels = px(6.);

pub enum PanelEvent {
    ZoomIn,
    ZoomOut,
    Activate,
    Close,
}

pub use proto::PanelId;

pub trait Panel: Focusable + EventEmitter<PanelEvent> + Render + Sized {
    fn persistent_name() -> &'static str;
    fn panel_key() -> &'static str;
    /// The `Focusable::focus_handle` root identifies the panel's subtree for containment checks
    /// and must be tracked by the panel's root element. This method returns the handle that should
    /// receive focus when the panel is activated, such as a filter, commit, or message editor; it
    /// must be a focus-tree descendant of the root or containment checks such as Zen-mode auto-close
    /// and toggle-focus will misbehave.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.focus_handle(cx)
    }
    fn position(&self, window: &Window, cx: &App) -> DockPosition;
    fn position_is_valid(&self, position: DockPosition) -> bool;
    fn set_position(&mut self, position: DockPosition, window: &mut Window, cx: &mut Context<Self>);
    fn default_size(&self, window: &Window, cx: &App) -> Pixels;
    fn min_size(&self, _window: &Window, _cx: &App) -> Option<Pixels> {
        None
    }
    fn initial_size_state(&self, _window: &Window, _cx: &App) -> PanelSizeState {
        PanelSizeState::default()
    }
    fn size_state_changed(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
    fn supports_flexible_size(&self) -> bool {
        false
    }
    fn has_flexible_size(&self, _window: &Window, _cx: &App) -> bool {
        false
    }
    fn set_flexible_size(
        &mut self,
        _flexible: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
    fn icon(&self, window: &Window, cx: &App) -> Option<ui::IconName>;
    fn icon_tooltip(&self, window: &Window, cx: &App) -> Option<&'static str>;
    fn toggle_action(&self) -> Box<dyn Action>;
    fn icon_label(&self, _window: &Window, _: &App) -> Option<String> {
        None
    }
    fn is_zoomed(&self, _window: &Window, _cx: &App) -> bool {
        false
    }
    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        false
    }
    fn set_zoomed(&mut self, _zoomed: bool, _window: &mut Window, _cx: &mut Context<Self>) {}
    fn set_active(&mut self, _active: bool, _window: &mut Window, _cx: &mut Context<Self>) {}
    fn pane(&self) -> Option<Entity<Pane>> {
        None
    }
    fn remote_id() -> Option<proto::PanelId> {
        None
    }
    fn activation_priority(&self) -> u32;
    fn enabled(&self, _cx: &App) -> bool {
        true
    }
    fn is_agent_panel(&self) -> bool {
        false
    }
    /// Returns metadata describing how to hide this panel's button from the
    /// status bar by writing to user settings. Implementors should return
    /// `None` if the panel button cannot be hidden through settings.
    fn hide_button_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

pub trait PanelHandle: Send + Sync {
    fn panel_id(&self) -> EntityId;
    fn persistent_name(&self) -> &'static str;
    fn panel_key(&self) -> &'static str;
    fn position(&self, window: &Window, cx: &App) -> DockPosition;
    fn position_is_valid(&self, position: DockPosition, cx: &App) -> bool;
    fn set_position(&self, position: DockPosition, window: &mut Window, cx: &mut App);
    fn is_zoomed(&self, window: &Window, cx: &App) -> bool;
    fn set_zoomed(&self, zoomed: bool, window: &mut Window, cx: &mut App);
    fn set_active(&self, active: bool, window: &mut Window, cx: &mut App);
    fn remote_id(&self) -> Option<proto::PanelId>;
    fn pane(&self, cx: &App) -> Option<Entity<Pane>>;
    fn default_size(&self, window: &Window, cx: &App) -> Pixels;
    fn min_size(&self, window: &Window, cx: &App) -> Option<Pixels>;
    fn initial_size_state(&self, window: &Window, cx: &App) -> PanelSizeState;
    fn size_state_changed(&self, window: &mut Window, cx: &mut App);
    fn supports_flexible_size(&self, cx: &App) -> bool;
    fn has_flexible_size(&self, window: &Window, cx: &App) -> bool;
    fn set_flexible_size(&self, flexible: bool, window: &mut Window, cx: &mut App);
    fn icon(&self, window: &Window, cx: &App) -> Option<ui::IconName>;
    fn icon_tooltip(&self, window: &Window, cx: &App) -> Option<&'static str>;
    fn toggle_action(&self, window: &Window, cx: &App) -> Box<dyn Action>;
    fn icon_label(&self, window: &Window, cx: &App) -> Option<String>;
    fn panel_focus_handle(&self, cx: &App) -> FocusHandle;
    /// See `Panel::activation_focus_handle`.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle;
    fn to_any(&self) -> AnyView;
    fn activation_priority(&self, cx: &App) -> u32;
    fn enabled(&self, cx: &App) -> bool;
    fn is_agent_panel(&self, cx: &App) -> bool;
    fn hide_button_setting(&self, cx: &App) -> Option<HideStatusItem>;
    fn move_to_next_position(&self, window: &mut Window, cx: &mut App) {
        let current_position = self.position(window, cx);
        let next_position = [
            DockPosition::Left,
            DockPosition::Bottom,
            DockPosition::Right,
        ]
        .into_iter()
        .filter(|position| self.position_is_valid(*position, cx))
        .skip_while(|valid_position| *valid_position != current_position)
        .nth(1)
        .unwrap_or(DockPosition::Left);

        self.set_position(next_position, window, cx);
    }
}

impl<T> PanelHandle for Entity<T>
where
    T: Panel,
{
    fn panel_id(&self) -> EntityId {
        Entity::entity_id(self)
    }

    fn persistent_name(&self) -> &'static str {
        T::persistent_name()
    }

    fn panel_key(&self) -> &'static str {
        T::panel_key()
    }

    fn position(&self, window: &Window, cx: &App) -> DockPosition {
        self.read(cx).position(window, cx)
    }

    fn position_is_valid(&self, position: DockPosition, cx: &App) -> bool {
        self.read(cx).position_is_valid(position)
    }

    fn set_position(&self, position: DockPosition, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_position(position, window, cx))
    }

    fn is_zoomed(&self, window: &Window, cx: &App) -> bool {
        self.read(cx).is_zoomed(window, cx)
    }

    fn set_zoomed(&self, zoomed: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_zoomed(zoomed, window, cx))
    }

    fn set_active(&self, active: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_active(active, window, cx))
    }

    fn pane(&self, cx: &App) -> Option<Entity<Pane>> {
        self.read(cx).pane()
    }

    fn remote_id(&self) -> Option<PanelId> {
        T::remote_id()
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        self.read(cx).default_size(window, cx)
    }

    fn min_size(&self, window: &Window, cx: &App) -> Option<Pixels> {
        self.read(cx).min_size(window, cx)
    }

    fn initial_size_state(&self, window: &Window, cx: &App) -> PanelSizeState {
        self.read(cx).initial_size_state(window, cx)
    }

    fn size_state_changed(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.size_state_changed(window, cx))
    }

    fn supports_flexible_size(&self, cx: &App) -> bool {
        self.read(cx).supports_flexible_size()
    }

    fn has_flexible_size(&self, window: &Window, cx: &App) -> bool {
        self.read(cx).has_flexible_size(window, cx)
    }

    fn set_flexible_size(&self, flexible: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_flexible_size(flexible, window, cx))
    }

    fn icon(&self, window: &Window, cx: &App) -> Option<ui::IconName> {
        self.read(cx).icon(window, cx)
    }

    fn icon_tooltip(&self, window: &Window, cx: &App) -> Option<&'static str> {
        self.read(cx).icon_tooltip(window, cx)
    }

    fn toggle_action(&self, _: &Window, cx: &App) -> Box<dyn Action> {
        self.read(cx).toggle_action()
    }

    fn icon_label(&self, window: &Window, cx: &App) -> Option<String> {
        self.read(cx).icon_label(window, cx)
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn panel_focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).activation_focus_handle(cx)
    }

    fn activation_priority(&self, cx: &App) -> u32 {
        self.read(cx).activation_priority()
    }

    fn enabled(&self, cx: &App) -> bool {
        self.read(cx).enabled(cx)
    }

    fn is_agent_panel(&self, cx: &App) -> bool {
        self.read(cx).is_agent_panel()
    }

    fn hide_button_setting(&self, cx: &App) -> Option<HideStatusItem> {
        self.read(cx).hide_button_setting(cx)
    }
}

impl From<&dyn PanelHandle> for AnyView {
    fn from(val: &dyn PanelHandle) -> Self {
        val.to_any()
    }
}

/// A container with a fixed [`DockPosition`] adjacent to a certain widown edge.
/// Can contain multiple panels and show/hide itself with all contents.
pub struct Dock {
    position: DockPosition,
    panel_entries: Vec<PanelEntry>,
    workspace: WeakEntity<Workspace>,
    is_open: bool,
    active_panel_index: Option<usize>,
    focus_handle: FocusHandle,
    focus_follows_mouse: FocusFollowsMouse,
    restoration: DockRestoreState,
    zoom_layer_open: bool,
    modal_layer: Entity<ModalLayer>,
    layout: DockLayout,
    /// The size of the area a split dock's panes share, as last drawn.
    pane_area: Option<gpui::Size<Pixels>>,
    /// Whether the window's saved layout has been looked for.
    pub(crate) layout_restored: bool,
    _subscriptions: [Subscription; 2],
}

enum DockRestoreState {
    Restoring { pending: Option<DockData> },
    Finished,
}

impl DockRestoreState {
    fn pending(&self) -> Option<&DockData> {
        match self {
            Self::Restoring { pending } => pending.as_ref(),
            Self::Finished => None,
        }
    }

    fn discard_pending(&mut self) {
        if let Self::Restoring { pending } = self {
            *pending = None;
        }
    }
}

impl Focusable for Dock {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DockPosition {
    Left,
    Bottom,
    Right,
}

impl From<settings::DockPosition> for DockPosition {
    fn from(value: settings::DockPosition) -> Self {
        match value {
            settings::DockPosition::Left => Self::Left,
            settings::DockPosition::Bottom => Self::Bottom,
            settings::DockPosition::Right => Self::Right,
        }
    }
}

impl Into<settings::DockPosition> for DockPosition {
    fn into(self) -> settings::DockPosition {
        match self {
            Self::Left => settings::DockPosition::Left,
            Self::Bottom => settings::DockPosition::Bottom,
            Self::Right => settings::DockPosition::Right,
        }
    }
}

impl From<TerminalDockPosition> for DockPosition {
    fn from(value: TerminalDockPosition) -> Self {
        match value {
            TerminalDockPosition::Left => DockPosition::Left,
            TerminalDockPosition::Bottom => DockPosition::Bottom,
            TerminalDockPosition::Right => DockPosition::Right,
        }
    }
}

impl DockPosition {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Bottom => "Bottom",
            Self::Right => "Right",
        }
    }

    pub fn axis(&self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Bottom => Axis::Vertical,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PanelSizeState {
    pub size: Option<Pixels>,
    #[serde(default)]
    pub flex: Option<f32>,
}

/// How a dock arranges its panels: in one pane, or in two stacked ones,
/// each with its panels in tab order and the one it shows. Panels the layout
/// doesn't name go at the end of the last pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DockLayout {
    pub panes: Vec<DockPaneLayout>,
    /// The first pane's share of the dock when it has two.
    pub divider: f32,
}

impl Default for DockLayout {
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            divider: DEFAULT_DIVIDER,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DockPaneLayout {
    /// Panels by persistent name, in tab order.
    pub panels: Vec<String>,
    /// The panel the pane shows, by persistent name.
    pub active: Option<String>,
}

/// Where a dragged tab is dropped: before another tab, at the end of a
/// pane's tabs, or in a new pane at the start or end of the dock. Panes are
/// counted as the dock shows them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TabDropTarget {
    Before { pane: usize, panel: EntityId },
    EndOf { pane: usize },
    NewPane { first: bool },
}

/// How much room a panel's pane should have along the dock's split.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PaneRoom {
    /// At least this much, tab row included.
    AtLeast(Pixels),
    /// As much as the other pane allows.
    Most,
}

/// The height of a pane's tab row.
const TAB_ROW_HEIGHT: Pixels = px(28.);
/// The space the divider between two panes takes.
const DIVIDER_SIZE: Pixels = px(5.);

/// The layouts docks start with in a window that has none saved, which an
/// app can set. Without them, a dock has one pane, as in Zed.
#[derive(Default)]
pub struct DefaultDockLayouts {
    pub left: Option<DockLayout>,
    pub right: Option<DockLayout>,
    pub bottom: Option<DockLayout>,
}

impl gpui::Global for DefaultDockLayouts {}

impl DefaultDockLayouts {
    pub fn for_position(&self, position: DockPosition) -> Option<&DockLayout> {
        match position {
            DockPosition::Left => self.left.as_ref(),
            DockPosition::Right => self.right.as_ref(),
            DockPosition::Bottom => self.bottom.as_ref(),
        }
    }
}

const DEFAULT_DIVIDER: f32 = 0.5;
/// Neither pane gets less than this share of a split dock.
const MIN_PANE_SHARE: f32 = 0.1;
pub(crate) const DOCK_LAYOUT_KEY: &str = "dock_layout";

/// A tab being dragged to another place in its dock.
#[derive(Clone)]
struct DraggedDockTab {
    position: DockPosition,
    panel_id: EntityId,
    icon: ui::IconName,
    label: SharedString,
}

impl Render for DraggedDockTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_1p5()
            .py_0p5()
            .gap_1()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_md()
            .child(Icon::new(self.icon).size(IconSize::Small))
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

/// What's dragged when the divider between a dock's panes moves.
#[derive(Clone)]
struct DraggedDockDivider(DockPosition);

impl Render for DraggedDockDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

struct PanelEntry {
    panel: Arc<dyn PanelHandle>,
    size_state: PanelSizeState,
    _subscriptions: [Subscription; 4],
}

pub struct PanelButtons {
    dock: Entity<Dock>,
    _settings_subscription: Subscription,
}

pub(crate) const PANEL_SIZE_STATE_KEY: &str = "dock_panel_size";

fn panel_uses_flexible_width(
    position: DockPosition,
    panel: &dyn PanelHandle,
    window: &Window,
    cx: &App,
) -> bool {
    position.axis() == Axis::Horizontal && panel.has_flexible_size(window, cx)
}

fn resize_panel_entry(
    position: DockPosition,
    entry: &mut PanelEntry,
    size: Option<Pixels>,
    flex: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> (&'static str, PanelSizeState) {
    let size = size.map(|size| size.max(RESIZE_HANDLE_SIZE).round());
    let uses_flexible_width = panel_uses_flexible_width(position, entry.panel.as_ref(), window, cx);
    if uses_flexible_width {
        entry.size_state.flex = flex;
    } else {
        entry.size_state.size = size;
    }
    entry.panel.size_state_changed(window, cx);
    (entry.panel.panel_key(), entry.size_state)
}

impl Dock {
    pub fn new(
        position: DockPosition,
        modal_layer: Entity<ModalLayer>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        let workspace = cx.entity();
        let dock = cx.new(|cx| {
            let focus_subscription =
                cx.on_focus(&focus_handle, window, |dock: &mut Dock, window, cx| {
                    if let Some(active_entry) = dock.active_panel_entry() {
                        active_entry
                            .panel
                            .activation_focus_handle(cx)
                            .focus(window, cx)
                    }
                });
            let zoom_subscription = cx.subscribe(&workspace, |dock, workspace, e: &Event, cx| {
                if matches!(e, Event::ZoomChanged) {
                    let is_zoomed = workspace.read(cx).zoomed.is_some();
                    dock.zoom_layer_open = is_zoomed;
                }
            });
            Self {
                position,
                workspace: workspace.downgrade(),
                panel_entries: Default::default(),
                active_panel_index: None,
                is_open: false,
                focus_handle: focus_handle.clone(),
                focus_follows_mouse: WorkspaceSettings::get_global(cx).focus_follows_mouse,
                _subscriptions: [focus_subscription, zoom_subscription],
                restoration: DockRestoreState::Restoring { pending: None },
                zoom_layer_open: false,
                modal_layer,
                layout: DockLayout::default(),
                pane_area: None,
                layout_restored: false,
            }
        });

        cx.on_focus_in(&focus_handle, window, {
            let dock = dock.downgrade();
            move |workspace, window, cx| {
                let Some(dock) = dock.upgrade() else {
                    return;
                };
                let Some(panel) = dock.read(cx).active_panel() else {
                    return;
                };
                if panel.is_zoomed(window, cx) {
                    workspace.zoomed = Some(panel.to_any().downgrade());
                    workspace.zoomed_position = Some(position);
                } else {
                    workspace.zoomed = None;
                    workspace.zoomed_position = None;
                }
                cx.emit(Event::ZoomChanged);
                workspace.dismiss_zoomed_items_to_reveal(Some(position), window, cx);
                workspace.update_active_view_for_followers(window, cx)
            }
        })
        .detach();

        cx.observe_in(&dock, window, move |workspace, dock, window, cx| {
            if dock.read(cx).is_open()
                && let Some(panel) = dock.read(cx).active_panel()
                && panel.is_zoomed(window, cx)
            {
                workspace.zoomed = Some(panel.to_any().downgrade());
                workspace.zoomed_position = Some(position);
                cx.emit(Event::ZoomChanged);
                return;
            }
            if workspace.zoomed_position == Some(position) {
                workspace.zoomed = None;
                workspace.zoomed_position = None;
                cx.emit(Event::ZoomChanged);
            }
        })
        .detach();

        dock
    }

    pub fn position(&self) -> DockPosition {
        self.position
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    fn resizable(&self, cx: &App) -> bool {
        !(self.zoom_layer_open || self.modal_layer.read(cx).has_active_modal())
    }

    pub fn panel<T: Panel>(&self) -> Option<Entity<T>> {
        self.panel_entries
            .iter()
            .find_map(|entry| entry.panel.to_any().downcast().ok())
    }

    pub fn panel_index_for_type<T: Panel>(&self) -> Option<usize> {
        self.panel_entries
            .iter()
            .position(|entry| entry.panel.to_any().downcast::<T>().is_ok())
    }

    pub fn panel_index_for_persistent_name(&self, ui_name: &str, _cx: &App) -> Option<usize> {
        self.panel_entries
            .iter()
            .position(|entry| entry.panel.persistent_name() == ui_name)
    }

    pub fn panel_index_for_proto_id(&self, panel_id: PanelId) -> Option<usize> {
        self.panel_entries
            .iter()
            .position(|entry| entry.panel.remote_id() == Some(panel_id))
    }

    pub fn panel_for_id(&self, panel_id: EntityId) -> Option<&Arc<dyn PanelHandle>> {
        self.panel_entries
            .iter()
            .find(|entry| entry.panel.panel_id() == panel_id)
            .map(|entry| &entry.panel)
    }

    pub fn first_enabled_panel_idx(&mut self, cx: &mut Context<Self>) -> anyhow::Result<usize> {
        self.panel_entries
            .iter()
            .position(|entry| entry.panel.enabled(cx))
            .with_context(|| {
                format!(
                    "Couldn't find any enabled panel for the {} dock.",
                    self.position.label()
                )
            })
    }

    fn active_panel_entry(&self) -> Option<&PanelEntry> {
        self.active_panel_index
            .and_then(|index| self.panel_entries.get(index))
    }

    fn active_panel_entry_mut(&mut self) -> Option<&mut PanelEntry> {
        self.active_panel_index
            .and_then(|index| self.panel_entries.get_mut(index))
    }

    pub fn active_panel_index(&self) -> Option<usize> {
        self.active_panel_index
    }

    pub fn set_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if open != self.is_open {
            self.restoration.discard_pending();
        }
        self.set_open_internal(open, window, cx);
    }

    fn set_open_internal(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if open != self.is_open {
            self.is_open = open;
            for ix in self.shown_indices() {
                if let Some(entry) = self.panel_entries.get(ix) {
                    entry.panel.set_active(open, window, cx);
                }
            }

            cx.notify();
        }
    }

    pub fn layout(&self) -> &DockLayout {
        &self.layout
    }

    /// Arranges the dock's panels as `layout` says, and saves it for the window.
    pub fn set_layout(&mut self, layout: DockLayout, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_layout(layout, window, cx);
        self.persist_layout(cx);
    }

    pub(crate) fn apply_layout(
        &mut self,
        layout: DockLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.shown_panel_ids();
        self.layout = layout;
        if self.is_split() {
            // Both panes fill the dock's one width.
            let size = self
                .active_panel_entry()
                .or_else(|| self.panel_entries.first())
                .map(|entry| {
                    entry
                        .size_state
                        .size
                        .unwrap_or_else(|| entry.panel.default_size(window, cx))
                });
            if size.is_some() {
                self.resize_all_panels(size, None, window, cx);
            }
        }
        self.update_shown(&before, None, window, cx);
        cx.notify();
    }

    fn persist_layout(&self, cx: &mut Context<Self>) {
        let position = self.position;
        let layout = self.layout.clone();
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.persist_dock_layout(position, &layout, cx);
                });
            }
        });
    }

    /// Each pane's panels, as indices into `panel_entries` in tab order,
    /// with the pane's index in the layout. A pane with no panels gives the
    /// dock to the other.
    fn panes(&self) -> Vec<(usize, Vec<usize>)> {
        let mut placed = vec![false; self.panel_entries.len()];
        let mut panes: Vec<(usize, Vec<usize>)> = self
            .layout
            .panes
            .iter()
            .enumerate()
            .map(|(pane_ix, pane)| {
                let panels = pane
                    .panels
                    .iter()
                    .filter_map(|name| {
                        let ix = self
                            .panel_entries
                            .iter()
                            .position(|entry| entry.panel.persistent_name() == name)?;
                        let already_placed = std::mem::replace(placed.get_mut(ix)?, true);
                        (!already_placed).then_some(ix)
                    })
                    .collect();
                (pane_ix, panels)
            })
            .collect();
        let unplaced: Vec<usize> = placed
            .iter()
            .enumerate()
            .filter(|(_, placed)| !**placed)
            .map(|(ix, _)| ix)
            .collect();
        match panes.last_mut() {
            Some((_, last)) => last.extend(unplaced),
            None => panes.push((0, unplaced)),
        }
        panes.retain(|(_, panels)| !panels.is_empty());
        panes
    }

    /// Whether the dock shows two panes.
    pub fn is_split(&self) -> bool {
        self.panes().len() > 1
    }

    /// The panel each pane shows, as indices, whether or not the dock is open.
    fn shown_indices(&self) -> Vec<usize> {
        let panes = self.panes();
        if panes.len() < 2 {
            return self.active_panel_index.into_iter().collect();
        }
        panes
            .iter()
            .filter_map(|(pane_ix, panels)| self.shown_in_pane(*pane_ix, panels))
            .collect()
    }

    fn shown_in_pane(&self, pane_ix: usize, panels: &[usize]) -> Option<usize> {
        let remembered = self
            .layout
            .panes
            .get(pane_ix)
            .and_then(|pane| pane.active.as_deref())
            .and_then(|name| {
                panels.iter().copied().find(|ix| {
                    self.panel_entries
                        .get(*ix)
                        .is_some_and(|entry| entry.panel.persistent_name() == name)
                })
            });
        remembered
            .or_else(|| self.active_panel_index.filter(|ix| panels.contains(ix)))
            .or_else(|| panels.first().copied())
    }

    fn shown_panel_ids(&self) -> Vec<EntityId> {
        self.shown_indices()
            .into_iter()
            .filter_map(|ix| self.panel_entries.get(ix))
            .map(|entry| entry.panel.panel_id())
            .collect()
    }

    /// The panels the dock shows: one per pane while it's open.
    pub fn visible_panels(&self) -> Vec<Arc<dyn PanelHandle>> {
        if !self.is_open {
            return Vec::new();
        }
        self.shown_indices()
            .into_iter()
            .filter_map(|ix| self.panel_entries.get(ix))
            .map(|entry| entry.panel.clone())
            .collect()
    }

    /// Tells panels whose showing changed since `before`. A panel shown
    /// because the layout changed, rather than because it was `chosen`, is
    /// only told while the dock is open.
    fn update_shown(
        &mut self,
        before: &[EntityId],
        chosen: Option<EntityId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let after = self.shown_panel_ids();
        for entry in &self.panel_entries {
            let id = entry.panel.panel_id();
            match (before.contains(&id), after.contains(&id)) {
                (true, false) => entry.panel.set_active(false, window, cx),
                (false, true) if self.is_open || chosen == Some(id) => {
                    entry.panel.set_active(true, window, cx)
                }
                _ => {}
            }
        }
    }

    /// Moves a panel to `target`, which shows it there. Emptying a pane gives
    /// the dock to the other; a new pane splits a dock that has one.
    pub fn move_panel(
        &mut self,
        panel_id: EntityId,
        target: TabDropTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name_of = |dock: &Self, id: EntityId| {
            dock.panel_entries
                .iter()
                .find(|entry| entry.panel.panel_id() == id)
                .map(|entry| entry.panel.persistent_name().to_string())
        };
        let Some(moved) = name_of(self, panel_id) else {
            return;
        };
        let effective = self.panes();
        let was_split = effective.len() > 1;
        let mut panes: Vec<(Vec<String>, Option<String>)> = effective
            .iter()
            .map(|(pane_ix, panels)| {
                let shown = if was_split {
                    self.shown_in_pane(*pane_ix, panels)
                } else {
                    self.active_panel_index
                };
                let name = |ix: usize| {
                    self.panel_entries
                        .get(ix)
                        .map(|entry| entry.panel.persistent_name().to_string())
                };
                (
                    panels.iter().filter_map(|ix| name(*ix)).collect(),
                    shown.and_then(name),
                )
            })
            .collect();
        for (panels, _) in &mut panes {
            panels.retain(|name| *name != moved);
        }
        match target {
            TabDropTarget::Before { pane, panel } => {
                let before = name_of(self, panel);
                let Some((panels, shown)) = panes.get_mut(pane) else {
                    return;
                };
                let at = before
                    .and_then(|before| panels.iter().position(|name| *name == before))
                    .unwrap_or(panels.len());
                panels.insert(at, moved.clone());
                *shown = Some(moved);
            }
            TabDropTarget::EndOf { pane } => {
                let Some((panels, shown)) = panes.get_mut(pane) else {
                    return;
                };
                panels.push(moved.clone());
                *shown = Some(moved);
            }
            TabDropTarget::NewPane { first } => {
                // A dock has at most two panes.
                if was_split {
                    return;
                }
                let pane = (vec![moved.clone()], Some(moved));
                if first {
                    panes.insert(0, pane);
                } else {
                    panes.push(pane);
                }
            }
        }
        panes.retain(|(panels, _)| !panels.is_empty());
        for (panels, shown) in &mut panes {
            if shown.as_ref().is_none_or(|shown| !panels.contains(shown)) {
                *shown = panels.first().cloned();
            }
        }
        let divider = if !was_split && panes.len() > 1 {
            DEFAULT_DIVIDER
        } else {
            self.layout.divider
        };
        let layout = DockLayout {
            panes: panes
                .into_iter()
                .map(|(panels, active)| DockPaneLayout { panels, active })
                .collect(),
            divider,
        };
        self.apply_layout(layout, window, cx);
        if let Some(ix) = self
            .panel_entries
            .iter()
            .position(|entry| entry.panel.panel_id() == panel_id)
        {
            self.activate_panel(ix, window, cx);
        }
        self.persist_layout(cx);
    }

    /// Moves the divider so the pane holding `panel_id` has `room`, as when
    /// an htmlwidget asks for a height. Does nothing in a dock with one pane.
    pub fn make_room_for(&mut self, panel_id: EntityId, room: PaneRoom, cx: &mut Context<Self>) {
        let panes = self.panes();
        if panes.len() < 2 {
            return;
        }
        let Some(pane_pos) = panes.iter().position(|(_, panels)| {
            panels.iter().any(|ix| {
                self.panel_entries
                    .get(*ix)
                    .is_some_and(|entry| entry.panel.panel_id() == panel_id)
            })
        }) else {
            return;
        };
        let share = match room {
            PaneRoom::Most => 1. - MIN_PANE_SHARE,
            PaneRoom::AtLeast(size) => {
                let Some(area) = self.pane_area else {
                    return;
                };
                let length = if self.position.axis() == Axis::Horizontal {
                    area.height
                } else {
                    area.width
                } - DIVIDER_SIZE;
                if length <= Pixels::ZERO {
                    return;
                }
                f32::from(size + TAB_ROW_HEIGHT) / f32::from(length)
            }
        }
        .clamp(MIN_PANE_SHARE, 1. - MIN_PANE_SHARE);
        // The divider gives the first pane its share and the second the rest.
        let divider = if pane_pos == 0 {
            self.layout.divider.max(share)
        } else {
            self.layout.divider.min(1. - share)
        };
        if divider != self.layout.divider {
            self.layout.divider = divider;
            self.persist_layout(cx);
            cx.notify();
        }
    }

    /// Remembers that `ix` is what its pane shows. Returns whether that changed.
    fn remember_shown(&mut self, ix: usize) -> bool {
        let Some(name) = self
            .panel_entries
            .get(ix)
            .map(|entry| entry.panel.persistent_name().to_string())
        else {
            return false;
        };
        let Some((pane_ix, _)) = self
            .panes()
            .into_iter()
            .find(|(_, panels)| panels.contains(&ix))
        else {
            return false;
        };
        let Some(pane) = self.layout.panes.get_mut(pane_ix) else {
            return false;
        };
        if pane.active.as_deref() == Some(name.as_str()) {
            return false;
        }
        pane.active = Some(name);
        true
    }

    pub fn set_panel_zoomed(
        &mut self,
        panel: &AnyView,
        zoomed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for entry in &mut self.panel_entries {
            if entry.panel.panel_id() == panel.entity_id() {
                if zoomed != entry.panel.is_zoomed(window, cx) {
                    entry.panel.set_zoomed(zoomed, window, cx);
                }
            } else if entry.panel.is_zoomed(window, cx) {
                entry.panel.set_zoomed(false, window, cx);
            }
        }

        self.workspace
            .update(cx, |workspace, cx| {
                workspace.serialize_workspace(window, cx);
            })
            .ok();
        cx.notify();
    }

    pub fn zoom_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for entry in &mut self.panel_entries {
            if entry.panel.is_zoomed(window, cx) {
                entry.panel.set_zoomed(false, window, cx);
            }
        }
    }

    pub(crate) fn add_panel<T: Panel>(
        &mut self,
        panel: Entity<T>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let subscriptions = [
            cx.observe(&panel, |_, _, cx| cx.notify()),
            cx.observe_global_in::<SettingsStore>(window, {
                let workspace = workspace.clone();
                let panel = panel.clone();

                move |this, window, cx| {
                    let new_position = panel.read(cx).position(window, cx);
                    if new_position == this.position {
                        return;
                    }

                    let Ok(new_dock) = workspace.update(cx, |workspace, cx| {
                        if panel.is_zoomed(window, cx) {
                            workspace.zoomed_position = Some(new_position);
                        }
                        match new_position {
                            DockPosition::Left => &workspace.left_dock,
                            DockPosition::Bottom => &workspace.bottom_dock,
                            DockPosition::Right => &workspace.right_dock,
                        }
                        .clone()
                    }) else {
                        return;
                    };

                    let panel_id = Entity::entity_id(&panel);
                    let was_visible = this.is_open()
                        && this
                            .visible_panel()
                            .is_some_and(|active_panel| active_panel.panel_id() == panel_id);
                    let size_state = this
                        .panel_entries
                        .iter()
                        .find(|entry| entry.panel.panel_id() == panel_id)
                        .map(|entry| entry.size_state)
                        .unwrap_or_default();

                    let previous_axis = this.position.axis();
                    let next_axis = new_position.axis();
                    let size_state = if previous_axis == next_axis {
                        size_state
                    } else {
                        PanelSizeState::default()
                    };

                    if !this.remove_panel(&panel, window, cx) {
                        // Panel was already moved from this dock
                        return;
                    }

                    new_dock.update(cx, |new_dock, cx| {
                        let index =
                            new_dock.add_panel(panel.clone(), workspace.clone(), window, cx);
                        if let Some(added_panel) = new_dock.panel_for_id(panel_id).cloned() {
                            new_dock.set_panel_size_state(added_panel.as_ref(), size_state, cx);
                        }
                        if was_visible {
                            new_dock.set_open(true, window, cx);
                            new_dock.activate_panel(index, window, cx);
                        }
                    });

                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.serialize_workspace(window, cx);
                        })
                        .ok();
                }
            }),
            {
                let panel = panel.clone();
                let mut last_default_size = panel.read(cx).default_size(window, cx);

                cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
                    let default_size = panel.read(cx).default_size(window, cx);
                    if default_size == last_default_size {
                        return;
                    }
                    last_default_size = default_size;

                    let panel_id = Entity::entity_id(&panel);
                    if let Some(entry) = this
                        .panel_entries
                        .iter_mut()
                        .find(|entry| entry.panel.panel_id() == panel_id)
                    {
                        entry.size_state.size = None;
                        entry.panel.size_state_changed(window, cx);
                        cx.notify();
                    }
                })
            },
            cx.subscribe_in(
                &panel,
                window,
                move |this, panel, event, window, cx| match event {
                    PanelEvent::ZoomIn => {
                        this.set_panel_zoomed(&panel.to_any(), true, window, cx);
                        if !PanelHandle::panel_focus_handle(panel, cx).contains_focused(window, cx)
                        {
                            window.focus(&panel.read(cx).activation_focus_handle(cx), cx);
                        }
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.zoomed = Some(panel.downgrade().into());
                                workspace.zoomed_position =
                                    Some(panel.read(cx).position(window, cx));
                                cx.emit(Event::ZoomChanged);
                            })
                            .ok();
                    }
                    PanelEvent::ZoomOut => {
                        this.set_panel_zoomed(&panel.to_any(), false, window, cx);
                        workspace
                            .update(cx, |workspace, cx| {
                                if workspace.zoomed_position == Some(this.position) {
                                    workspace.zoomed = None;
                                    workspace.zoomed_position = None;
                                    cx.emit(Event::ZoomChanged);
                                }
                                cx.notify();
                            })
                            .ok();
                    }
                    PanelEvent::Activate => {
                        if let Some(ix) = this
                            .panel_entries
                            .iter()
                            .position(|entry| entry.panel.panel_id() == Entity::entity_id(panel))
                        {
                            this.set_open(true, window, cx);
                            this.activate_panel(ix, window, cx);
                            window.focus(&panel.read(cx).activation_focus_handle(cx), cx);
                        }
                    }
                    PanelEvent::Close => {
                        if this
                            .visible_panel()
                            .is_some_and(|p| p.panel_id() == Entity::entity_id(panel))
                        {
                            this.set_open(false, window, cx);
                        }
                    }
                },
            ),
        ];

        let index = match self
            .panel_entries
            .binary_search_by_key(&panel.read(cx).activation_priority(), |entry| {
                entry.panel.activation_priority(cx)
            }) {
            Ok(ix) => {
                if cfg!(debug_assertions) {
                    panic!(
                        "Panels `{}` and `{}` have the same activation priority. Each panel must have a unique priority so the status bar order is deterministic.",
                        T::panel_key(),
                        self.panel_entries[ix].panel.panel_key()
                    );
                }
                ix
            }
            Err(ix) => ix,
        };
        if let Some(active_index) = self.active_panel_index.as_mut()
            && *active_index >= index
        {
            *active_index += 1;
        }
        let size_state = panel.read(cx).initial_size_state(window, cx);
        let shown_before = self.shown_panel_ids();

        self.panel_entries.insert(
            index,
            PanelEntry {
                panel: Arc::new(panel.clone()),
                size_state,
                _subscriptions: subscriptions,
            },
        );
        // A panel the layout puts in an empty pane shows there.
        self.update_shown(&shown_before, None, window, cx);

        self.replay_pending_serialized_state(window, cx);

        if panel.read(cx).starts_open(window, cx) {
            self.activate_panel_internal(index, window, cx);
            self.set_open_internal(true, window, cx);
        }

        cx.notify();
        index
    }

    pub(crate) fn restore_serialized_state(
        &mut self,
        serialized: DockData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &mut self.restoration {
            DockRestoreState::Restoring { pending } => {
                *pending = Some(serialized);
                self.replay_pending_serialized_state(window, cx);
            }
            DockRestoreState::Finished => {
                let active_panel_missing = serialized
                    .active_panel
                    .as_deref()
                    .filter(|_| serialized.visible)
                    .is_some_and(|name| self.panel_index_for_persistent_name(name, cx).is_none());
                if !active_panel_missing {
                    self.apply_serialized_state(&serialized, window, cx);
                }
            }
        }
    }

    fn replay_pending_serialized_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(serialized) = self.restoration.pending().cloned() else {
            return;
        };
        let waiting_for_active_panel = serialized
            .active_panel
            .as_deref()
            .filter(|_| serialized.visible)
            .is_some_and(|name| self.panel_index_for_persistent_name(name, cx).is_none());
        if waiting_for_active_panel {
            self.set_open_internal(serialized.visible, window, cx);
        } else {
            self.apply_serialized_state(&serialized, window, cx);
            self.restoration.discard_pending();
        }
    }

    fn apply_serialized_state(
        &mut self,
        serialized: &DockData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active_panel) = serialized
            .active_panel
            .as_deref()
            .filter(|_| serialized.visible)
            && let Some(idx) = self.panel_index_for_persistent_name(active_panel, cx)
        {
            self.activate_panel_internal(idx, window, cx);
        }
        if serialized.zoom
            && let Some(panel) = self.active_panel()
        {
            panel.set_zoomed(true, window, cx)
        }
        self.set_open_internal(serialized.visible, window, cx);
    }

    pub(crate) fn finish_restoration(&mut self) {
        self.restoration = DockRestoreState::Finished;
    }

    pub fn remove_panel<T: Panel>(
        &mut self,
        panel: &Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(panel_ix) = self
            .panel_entries
            .iter()
            .position(|entry| entry.panel.panel_id() == Entity::entity_id(panel))
        {
            let shown_before = self.shown_panel_ids();
            if let Some(active_panel_index) = self.active_panel_index.as_mut() {
                match panel_ix.cmp(active_panel_index) {
                    std::cmp::Ordering::Less => {
                        *active_panel_index -= 1;
                    }
                    std::cmp::Ordering::Equal => {
                        self.active_panel_index = None;
                        self.set_open(false, window, cx);
                    }
                    std::cmp::Ordering::Greater => {}
                }
            }

            self.panel_entries.remove(panel_ix);
            // The other panels in its pane may show instead.
            self.update_shown(&shown_before, None, window, cx);
            cx.notify();

            true
        } else {
            false
        }
    }

    pub fn panels_len(&self) -> usize {
        self.panel_entries.len()
    }

    pub fn has_agent_panel(&self, cx: &App) -> bool {
        self.panel_entries
            .iter()
            .any(|entry| entry.panel.is_agent_panel(cx))
    }

    pub fn activate_panel(&mut self, panel_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if Some(panel_ix) != self.active_panel_index {
            self.restoration.discard_pending();
        }
        self.activate_panel_internal(panel_ix, window, cx);
    }

    fn activate_panel_internal(
        &mut self,
        panel_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only the panel the other pane shows stays shown.
        let shown_before = self.shown_panel_ids();
        let index_changed = Some(panel_ix) != self.active_panel_index;
        self.active_panel_index = Some(panel_ix);
        let layout_changed = self.remember_shown(panel_ix);
        let chosen = self
            .panel_entries
            .get(panel_ix)
            .map(|entry| entry.panel.panel_id());
        self.update_shown(&shown_before, chosen, window, cx);
        if index_changed || layout_changed {
            cx.notify();
        }
        if layout_changed {
            self.persist_layout(cx);
        }
    }

    pub fn visible_panel(&self) -> Option<&Arc<dyn PanelHandle>> {
        let entry = self.visible_entry()?;
        Some(&entry.panel)
    }

    pub fn active_panel(&self) -> Option<&Arc<dyn PanelHandle>> {
        let panel_entry = self.active_panel_entry()?;
        Some(&panel_entry.panel)
    }

    fn visible_entry(&self) -> Option<&PanelEntry> {
        if self.is_open {
            self.active_panel_entry()
        } else {
            None
        }
    }

    pub fn zoomed_panel(&self, window: &Window, cx: &App) -> Option<Arc<dyn PanelHandle>> {
        let entry = self.visible_entry()?;
        if entry.panel.is_zoomed(window, cx) {
            Some(entry.panel.clone())
        } else {
            None
        }
    }

    pub fn active_panel_size(&self) -> Option<PanelSizeState> {
        if self.is_open {
            self.active_panel_entry().map(|entry| entry.size_state)
        } else {
            None
        }
    }

    pub fn stored_panel_size(
        &self,
        panel: &dyn PanelHandle,
        window: &Window,
        cx: &App,
    ) -> Option<Pixels> {
        self.panel_entries
            .iter()
            .find(|entry| entry.panel.panel_id() == panel.panel_id())
            .map(|entry| {
                entry
                    .size_state
                    .size
                    .unwrap_or_else(|| entry.panel.default_size(window, cx))
            })
    }

    pub fn stored_panel_size_state(&self, panel: &dyn PanelHandle) -> Option<PanelSizeState> {
        self.panel_entries
            .iter()
            .find(|entry| entry.panel.panel_id() == panel.panel_id())
            .map(|entry| entry.size_state)
    }

    pub fn stored_active_panel_size(&self, window: &Window, cx: &App) -> Option<Pixels> {
        if self.is_open {
            self.active_panel_entry().map(|entry| {
                entry
                    .size_state
                    .size
                    .unwrap_or_else(|| entry.panel.default_size(window, cx))
            })
        } else {
            None
        }
    }

    pub fn set_panel_size_state(
        &mut self,
        panel: &dyn PanelHandle,
        size_state: PanelSizeState,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(entry) = self
            .panel_entries
            .iter_mut()
            .find(|entry| entry.panel.panel_id() == panel.panel_id())
        {
            entry.size_state = size_state;
            cx.notify();
            true
        } else {
            false
        }
    }

    pub fn toggle_panel_flexible_size(
        &mut self,
        panel: &dyn PanelHandle,
        current_size: Option<Pixels>,
        current_flex: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .panel_entries
            .iter_mut()
            .find(|entry| entry.panel.panel_id() == panel.panel_id())
        else {
            return;
        };
        let currently_flexible = entry.panel.has_flexible_size(window, cx);
        if currently_flexible {
            entry.size_state.size = current_size;
        } else {
            entry.size_state.flex = current_flex;
        }
        let panel_key = entry.panel.panel_key();
        let size_state = entry.size_state;
        let workspace = self.workspace.clone();
        entry
            .panel
            .set_flexible_size(!currently_flexible, window, cx);
        entry.panel.size_state_changed(window, cx);
        cx.defer(move |cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.persist_panel_size_state(panel_key, size_state, cx);
                });
            }
        });
        cx.notify();
    }

    fn resize_active_panel(
        &mut self,
        size: Option<Pixels>,
        flex: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = self.position;
        if let Some(entry) = self.active_panel_entry_mut() {
            let (panel_key, size_state) =
                resize_panel_entry(position, entry, size, flex, window, cx);

            let workspace = self.workspace.clone();
            cx.defer(move |cx| {
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.persist_panel_size_state(panel_key, size_state, cx);
                    });
                }
            });
            cx.notify();
        }
    }

    /// Resizes the active panel and, when this dock is included in
    /// `resize_all_panels_in_dock`, all panels using the same sizing mode.
    pub fn resize_panel_sizes(
        &mut self,
        size: Option<Pixels>,
        flex: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.should_resize_all_panels(cx) {
            self.resize_all_panels(size, flex, window, cx);
        } else {
            self.resize_active_panel(size, flex, window, cx);
        }
    }

    /// Resets the active panel and, when this dock is included in
    /// `resize_all_panels_in_dock`, all panels using the same sizing mode.
    pub fn reset_panel_sizes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.should_resize_all_panels(cx) {
            self.reset_all_panel_sizes(window, cx);
        } else {
            self.reset_active_panel_size(window, cx);
        }
    }

    fn reset_active_panel_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = PanelSizeState::default();
        self.resize_active_panel(state.size, state.flex, window, cx);
    }

    fn reset_all_panel_sizes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active_entry) = self.active_panel_entry() else {
            return;
        };
        let size =
            (!panel_uses_flexible_width(self.position, active_entry.panel.as_ref(), window, cx))
                .then(|| active_entry.panel.default_size(window, cx));
        self.resize_all_panels(size, None, window, cx);
    }

    fn should_resize_all_panels(&self, cx: &App) -> bool {
        // A split dock's panes share its width.
        self.is_split()
            || WorkspaceSettings::get_global(cx)
                .resize_all_panels_in_dock
                .contains(&self.position)
    }

    fn resize_all_panels(
        &mut self,
        size: Option<Pixels>,
        flex: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_panel_index) = self.active_panel_index else {
            return;
        };

        let active_panel_uses_flexible_width = {
            let Some(active_entry) = self.panel_entries.get(active_panel_index) else {
                return;
            };
            panel_uses_flexible_width(self.position, active_entry.panel.as_ref(), window, cx)
        };
        let mut size_states_to_persist = Vec::new();
        for entry in &mut self.panel_entries {
            if panel_uses_flexible_width(self.position, entry.panel.as_ref(), window, cx)
                == active_panel_uses_flexible_width
            {
                size_states_to_persist.push(resize_panel_entry(
                    self.position,
                    entry,
                    size,
                    flex,
                    window,
                    cx,
                ));
            }
        }

        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    for (panel_key, size_state) in size_states_to_persist {
                        workspace.persist_panel_size_state(panel_key, size_state, cx);
                    }
                });
            }
        });

        cx.notify();
    }

    pub fn toggle_action(&self) -> Box<dyn Action> {
        match self.position {
            DockPosition::Left => crate::ToggleLeftDock.boxed_clone(),
            DockPosition::Bottom => crate::ToggleBottomDock.boxed_clone(),
            DockPosition::Right => crate::ToggleRightDock.boxed_clone(),
        }
    }

    fn dispatch_context(cx: &App) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults(cx.platform_info().os);
        dispatch_context.add("Dock");

        dispatch_context
    }

    pub fn clamp_panel_size(&mut self, max_size: Pixels, window: &Window, cx: &mut Context<Self>) {
        let max_size = (max_size - RESIZE_HANDLE_SIZE).abs();
        let mut clamped = false;
        for entry in &mut self.panel_entries {
            let uses_flexible_width =
                panel_uses_flexible_width(self.position, entry.panel.as_ref(), window, cx);
            if uses_flexible_width {
                continue;
            }

            let size = entry
                .size_state
                .size
                .unwrap_or_else(|| entry.panel.default_size(window, cx));
            if size > max_size {
                entry.size_state.size = Some(max_size.max(RESIZE_HANDLE_SIZE));
                clamped = true;
            }
        }
        if clamped {
            cx.notify();
        }
    }

    pub(crate) fn load_persisted_layout(
        workspace: &Workspace,
        position: DockPosition,
        cx: &App,
    ) -> Option<DockLayout> {
        let workspace_id = workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())?;
        let kvp = KeyValueStore::global(cx);
        let scope = kvp.scoped(DOCK_LAYOUT_KEY);
        scope
            .read(&format!("{workspace_id}:{}", position.label()))
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<DockLayout>(&json).log_err())
    }

    pub(crate) fn load_persisted_size_state(
        workspace: &Workspace,
        panel_key: &'static str,
        cx: &App,
    ) -> Option<PanelSizeState> {
        let workspace_id = workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())?;
        let kvp = KeyValueStore::global(cx);
        let scope = kvp.scoped(PANEL_SIZE_STATE_KEY);
        scope
            .read(&format!("{workspace_id}:{panel_key}"))
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<PanelSizeState>(&json).log_err())
    }
}

impl Dock {
    fn render_panes(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let panes = self.panes();
        let split = panes.len() > 1;
        if !split {
            return panes
                .first()
                .and_then(|(_, panels)| {
                    let shown = self.active_panel_index?;
                    Some(self.render_pane(0, panels, shown, false, window, cx))
                })
                .unwrap_or_else(|| gpui::Empty.into_any_element());
        }
        let mut pane_elements =
            panes
                .iter()
                .enumerate()
                .filter_map(|(pane_pos, (pane_ix, panels))| {
                    let shown = self.shown_in_pane(*pane_ix, panels)?;
                    Some(self.render_pane(pane_pos, panels, shown, true, window, cx))
                });
        let (Some(first), Some(second)) = (pane_elements.next(), pane_elements.next()) else {
            return gpui::Empty.into_any_element();
        };

        let position = self.position;
        // Side docks stack their panes; the bottom dock puts them side by side.
        let stacked = position.axis() == Axis::Horizontal;
        let share = self
            .layout
            .divider
            .clamp(MIN_PANE_SHARE, 1. - MIN_PANE_SHARE);
        let divider = div()
            .id("dock-pane-divider")
            .flex_none()
            .flex()
            .justify_center()
            .items_center()
            .map(|this| {
                if stacked {
                    this.w_full().h(DIVIDER_SIZE).flex_col().cursor_row_resize()
                } else {
                    this.h_full().w(DIVIDER_SIZE).flex_row().cursor_col_resize()
                }
            })
            .child(div().bg(cx.theme().colors().border).map(|this| {
                if stacked {
                    this.w_full().h(px(1.))
                } else {
                    this.h_full().w(px(1.))
                }
            }))
            .on_drag(DraggedDockDivider(position), |divider, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| divider.clone())
            })
            .on_click(cx.listener(|dock, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    dock.layout.divider = DEFAULT_DIVIDER;
                    dock.persist_layout(cx);
                    cx.notify();
                }
            }));

        let dock = cx.entity();
        div()
            .size_full()
            .flex()
            .relative()
            .map(|this| {
                if stacked {
                    this.flex_col()
                } else {
                    this.flex_row()
                }
            })
            .child(
                gpui::canvas(
                    move |bounds, _, cx| {
                        dock.update(cx, |dock, _| dock.pane_area = Some(bounds.size));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_drag_move(cx.listener(
                move |dock, event: &gpui::DragMoveEvent<DraggedDockDivider>, _, cx| {
                    if event.drag(cx).0 != dock.position {
                        return;
                    }
                    let bounds = event.bounds;
                    let share = if stacked {
                        f32::from(event.event.position.y - bounds.top())
                            / f32::from(bounds.size.height)
                    } else {
                        f32::from(event.event.position.x - bounds.left())
                            / f32::from(bounds.size.width)
                    };
                    if share.is_finite() {
                        dock.layout.divider = share.clamp(MIN_PANE_SHARE, 1. - MIN_PANE_SHARE);
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|dock, _: &DraggedDockDivider, _, cx| {
                dock.persist_layout(cx);
            }))
            .child(
                div()
                    .flex_none()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .map(|this| {
                        if stacked {
                            this.w_full().h(relative(share))
                        } else {
                            this.h_full().w(relative(share))
                        }
                    })
                    .child(first),
            )
            .child(divider)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(second),
            )
            .into_any_element()
    }

    /// A pane: a tab row when it has more than one panel, and the panel it
    /// shows. While a tab is dragged, the panel's area takes drops: into this
    /// pane when the dock is split, or into a new pane at either end when not.
    fn render_pane(
        &self,
        pane_pos: usize,
        panels: &[usize],
        shown: usize,
        split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.panel_entries.get(shown) else {
            return gpui::Empty.into_any_element();
        };
        let dragging_tab = cx.has_active_drag();
        let drop_highlight = cx.theme().colors().drop_target_background;
        let stacked = self.position.axis() == Axis::Horizontal;
        let new_pane_zone = |first: bool, cx: &mut Context<Self>| {
            div()
                .id(("dock-new-pane", usize::from(first)))
                .absolute()
                .map(|this| match (stacked, first) {
                    (true, true) => this.top_0().left_0().w_full().h(relative(0.3)),
                    (true, false) => this.bottom_0().left_0().w_full().h(relative(0.3)),
                    (false, true) => this.left_0().top_0().h_full().w(relative(0.3)),
                    (false, false) => this.right_0().top_0().h_full().w(relative(0.3)),
                })
                .drag_over::<DraggedDockTab>(move |style, _, _, _| style.bg(drop_highlight))
                .on_drop(cx.listener(move |dock, tab: &DraggedDockTab, window, cx| {
                    if tab.position == dock.position {
                        dock.move_panel(tab.panel_id, TabDropTarget::NewPane { first }, window, cx);
                    }
                }))
        };
        v_flex()
            .size_full()
            .min_h_0()
            .when(panels.len() > 1, |this| {
                this.child(self.render_tab_row(pane_pos, panels, shown, window, cx))
            })
            .child(
                div()
                    .id(("dock-pane", pane_pos))
                    .debug_selector(move || format!("dock-pane-{pane_pos}"))
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        entry
                            .panel
                            .to_any()
                            .cached(StyleRefinement::default().v_flex().size_full()),
                    )
                    .when(split, |this| {
                        this.drag_over::<DraggedDockTab>(move |style, _, _, _| {
                            style.bg(drop_highlight)
                        })
                        .on_drop(cx.listener(
                            move |dock, tab: &DraggedDockTab, window, cx| {
                                if tab.position == dock.position {
                                    dock.move_panel(
                                        tab.panel_id,
                                        TabDropTarget::EndOf { pane: pane_pos },
                                        window,
                                        cx,
                                    );
                                }
                            },
                        ))
                    })
                    // A dock with one pane splits when a tab is dropped near an end.
                    .when(!split && dragging_tab && panels.len() > 1, |this| {
                        this.child(new_pane_zone(true, cx))
                            .child(new_pane_zone(false, cx))
                    }),
            )
            .into_any_element()
    }

    fn render_tab_row(
        &self,
        pane_pos: usize,
        panels: &[usize],
        shown: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (selected_bg, hover_bg, border, drop_line, drop_highlight) = (
            colors.element_selected,
            colors.element_hover,
            colors.border_variant,
            colors.text_accent,
            colors.drop_target_background,
        );
        let position = self.position;
        h_flex()
            .w_full()
            .flex_none()
            .h(TAB_ROW_HEIGHT)
            .px_1()
            .gap_0p5()
            .overflow_x_hidden()
            .border_b_1()
            .border_color(border)
            .children(panels.iter().filter_map(|&ix| {
                let entry = self.panel_entries.get(ix)?;
                let icon = entry.panel.icon(window, cx)?;
                let label = tab_label(entry.panel.as_ref(), window, cx);
                let selected = ix == shown;
                let color = if selected {
                    Color::Default
                } else {
                    Color::Muted
                };
                let panel_id = entry.panel.panel_id();
                Some(
                    h_flex()
                        .id(("dock-tab", ix))
                        .debug_selector({
                            let label = label.clone();
                            move || format!("dock-tab-{label}")
                        })
                        .flex_none()
                        .px_1p5()
                        .py_0p5()
                        .gap_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(selected, |this| this.bg(selected_bg))
                        .hover(move |style| style.bg(hover_bg))
                        .child(Icon::new(icon).size(IconSize::Small).color(color))
                        .child(
                            Label::new(label.clone())
                                .size(LabelSize::Small)
                                .color(color),
                        )
                        .on_click(cx.listener(move |dock, _, window, cx| {
                            dock.show_panel_from_tab(panel_id, window, cx)
                        }))
                        .on_drag(
                            DraggedDockTab {
                                position,
                                panel_id,
                                icon,
                                label,
                            },
                            |tab, _, _, cx| {
                                cx.stop_propagation();
                                cx.new(|_| tab.clone())
                            },
                        )
                        // A line at the tab's start shows where the dragged one lands.
                        .drag_over::<DraggedDockTab>(move |style, _, _, _| {
                            style.border_l_2().border_color(drop_line)
                        })
                        .on_drop(cx.listener(move |dock, tab: &DraggedDockTab, window, cx| {
                            if tab.position == dock.position && tab.panel_id != panel_id {
                                dock.move_panel(
                                    tab.panel_id,
                                    TabDropTarget::Before {
                                        pane: pane_pos,
                                        panel: panel_id,
                                    },
                                    window,
                                    cx,
                                );
                            }
                        })),
                )
            }))
            .child(
                div()
                    .id(("dock-tab-row-end", pane_pos))
                    .flex_1()
                    .h_full()
                    .drag_over::<DraggedDockTab>(move |style, _, _, _| style.bg(drop_highlight))
                    .on_drop(cx.listener(move |dock, tab: &DraggedDockTab, window, cx| {
                        if tab.position == dock.position {
                            dock.move_panel(
                                tab.panel_id,
                                TabDropTarget::EndOf { pane: pane_pos },
                                window,
                                cx,
                            );
                        }
                    })),
            )
    }

    pub(crate) fn show_panel_from_tab(
        &mut self,
        panel_id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self
            .panel_entries
            .iter()
            .position(|entry| entry.panel.panel_id() == panel_id)
        else {
            return;
        };
        self.activate_panel(ix, window, cx);
        if let Some(entry) = self.panel_entries.get(ix) {
            window.focus(&entry.panel.activation_focus_handle(cx), cx);
        }
    }
}

/// A panel's tab says what its status bar button's tooltip says, without
/// "Panel": "Project Panel" is "Project".
fn tab_label(panel: &dyn PanelHandle, window: &Window, cx: &App) -> SharedString {
    let name = panel
        .icon_tooltip(window, cx)
        .unwrap_or_else(|| panel.persistent_name());
    SharedString::from(name.strip_suffix(" Panel").unwrap_or(name).to_string())
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dispatch_context = Self::dispatch_context(cx);
        if self.visible_entry().is_some() {
            let panes = self.render_panes(window, cx);
            let position = self.position;
            let create_resize_handle = || {
                let handle = div()
                    .id("resize-handle")
                    .on_drag(DraggedDock(position), |dock, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| dock.clone())
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|dock, e: &MouseUpEvent, window, cx| {
                            if e.click_count == 2 {
                                dock.reset_panel_sizes(window, cx);
                                dock.workspace
                                    .update(cx, |workspace, cx| {
                                        workspace.serialize_workspace(window, cx);
                                    })
                                    .ok();
                                cx.stop_propagation();
                            }
                        }),
                    )
                    .occlude();
                match self.position() {
                    DockPosition::Left => deferred(
                        handle
                            .absolute()
                            .right(-RESIZE_HANDLE_SIZE / 2.)
                            .top(px(0.))
                            .h_full()
                            .w(RESIZE_HANDLE_SIZE)
                            .cursor_col_resize(),
                    ),
                    DockPosition::Bottom => deferred(
                        handle
                            .absolute()
                            .top(-RESIZE_HANDLE_SIZE / 2.)
                            .left(px(0.))
                            .w_full()
                            .h(RESIZE_HANDLE_SIZE)
                            .cursor_row_resize(),
                    ),
                    DockPosition::Right => deferred(
                        handle
                            .absolute()
                            .top(px(0.))
                            .left(-RESIZE_HANDLE_SIZE / 2.)
                            .h_full()
                            .w(RESIZE_HANDLE_SIZE)
                            .cursor_col_resize(),
                    ),
                }
            };

            div()
                .id("dock-panel")
                .key_context(dispatch_context)
                .track_focus(&self.focus_handle(cx))
                .focus_follows_mouse(self.focus_follows_mouse, cx)
                .flex()
                .bg(cx.theme().colors().panel_background)
                .border_color(cx.theme().colors().border)
                .overflow_hidden()
                .map(|this| match self.position().axis() {
                    // Width and height are always set on the workspace wrapper in
                    // render_dock, so fill whatever space the wrapper provides.
                    Axis::Horizontal => this.w_full().h_full().flex_row(),
                    Axis::Vertical => this.h_full().w_full().flex_col(),
                })
                .map(|this| match self.position() {
                    DockPosition::Left => this.border_r_1(),
                    DockPosition::Right => this.border_l_1(),
                    DockPosition::Bottom => this.border_t_1(),
                })
                .child(
                    div()
                        .map(|this| match self.position().axis() {
                            Axis::Horizontal => this.w_full().h_full(),
                            Axis::Vertical => this.h_full().w_full(),
                        })
                        .child(panes),
                )
                .when(self.resizable(cx), |this| {
                    this.child(create_resize_handle())
                })
        } else {
            div()
                .id("dock-panel")
                .key_context(dispatch_context)
                .track_focus(&self.focus_handle(cx))
        }
    }
}

impl PanelButtons {
    pub fn new(dock: Entity<Dock>, cx: &mut Context<Self>) -> Self {
        cx.observe(&dock, |_, _, cx| cx.notify()).detach();
        let settings_subscription = cx.observe_global::<SettingsStore>(|_, cx| cx.notify());
        Self {
            dock,
            _settings_subscription: settings_subscription,
        }
    }
}

impl Render for PanelButtons {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dock = self.dock.read(cx);
        let shown = dock.shown_indices();
        let is_open = dock.is_open;
        let dock_position = dock.position;

        let (menu_anchor, menu_attach) = match dock.position {
            DockPosition::Left => (Anchor::BottomLeft, Anchor::TopLeft),
            DockPosition::Bottom | DockPosition::Right => (Anchor::BottomRight, Anchor::TopRight),
        };

        let dock_entity = self.dock.clone();
        let workspace = dock.workspace.clone();
        let mut buttons: Vec<_> = dock
            .panel_entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let icon = entry.panel.icon(window, cx)?;
                let icon_tooltip = entry
                    .panel
                    .icon_tooltip(window, cx)
                    .ok_or_else(|| {
                        anyhow::anyhow!("can't render a panel button without an icon tooltip")
                    })
                    .log_err()?;
                let name = entry.panel.persistent_name();
                let panel = entry.panel.clone();
                let supports_flexible = panel.supports_flexible_size(cx);
                let currently_flexible = panel.has_flexible_size(window, cx);
                let dock_for_menu = dock_entity.clone();
                let workspace_for_menu = workspace.clone();

                let is_active_button = shown.contains(&i) && is_open;
                let (action, tooltip) = if is_active_button {
                    let action = dock.toggle_action();

                    let tooltip: SharedString =
                        format!("Close {} Dock", dock.position.label()).into();

                    (action, tooltip)
                } else {
                    let action = entry.panel.toggle_action(window, cx);

                    (action, icon_tooltip.into())
                };

                let focus_handle = dock.focus_handle(cx);
                let icon_label = entry.panel.icon_label(window, cx);

                Some(
                    right_click_menu(name)
                        .menu(move |window, cx| {
                            const POSITIONS: [DockPosition; 3] = [
                                DockPosition::Left,
                                DockPosition::Right,
                                DockPosition::Bottom,
                            ];

                            let panel_hide = panel.hide_button_setting(cx);
                            ContextMenu::build(window, cx, |mut menu, _, cx| {
                                let mut has_position_entries = false;
                                for position in POSITIONS {
                                    if panel.position_is_valid(position, cx) {
                                        let is_current = position == dock_position;
                                        let panel = panel.clone();
                                        menu = menu.toggleable_entry(
                                            format!("Dock {}", position.label()),
                                            is_current,
                                            IconPosition::Start,
                                            None,
                                            move |window, cx| {
                                                if !is_current {
                                                    panel.set_position(position, window, cx);
                                                }
                                            },
                                        );
                                        has_position_entries = true;
                                    }
                                }
                                if supports_flexible {
                                    if has_position_entries {
                                        menu = menu.separator();
                                    }
                                    let panel_for_flex = panel.clone();
                                    let dock_for_flex = dock_for_menu.clone();
                                    let workspace_for_flex = workspace_for_menu.clone();
                                    menu = menu.toggleable_entry(
                                        "Flex Width",
                                        currently_flexible,
                                        IconPosition::Start,
                                        None,
                                        move |window, cx| {
                                            if !currently_flexible {
                                                if let Some(ws) = workspace_for_flex.upgrade() {
                                                    ws.update(cx, |workspace, cx| {
                                                        workspace.toggle_dock_panel_flexible_size(
                                                            &dock_for_flex,
                                                            panel_for_flex.as_ref(),
                                                            window,
                                                            cx,
                                                        );
                                                    });
                                                }
                                            }
                                        },
                                    );
                                    let panel_for_fixed = panel.clone();
                                    let dock_for_fixed = dock_for_menu.clone();
                                    let workspace_for_fixed = workspace_for_menu.clone();
                                    menu = menu.toggleable_entry(
                                        "Fixed Width",
                                        !currently_flexible,
                                        IconPosition::Start,
                                        None,
                                        move |window, cx| {
                                            if currently_flexible {
                                                if let Some(ws) = workspace_for_fixed.upgrade() {
                                                    ws.update(cx, |workspace, cx| {
                                                        workspace.toggle_dock_panel_flexible_size(
                                                            &dock_for_fixed,
                                                            panel_for_fixed.as_ref(),
                                                            window,
                                                            cx,
                                                        );
                                                    });
                                                }
                                            }
                                        },
                                    );
                                }
                                if let Some(hide) = panel_hide {
                                    menu = crate::status_bar::add_hide_button_entry(
                                        menu.separator(),
                                        hide,
                                    );
                                }
                                menu
                            })
                        })
                        .anchor(menu_anchor)
                        .attach(menu_attach)
                        .trigger(move |is_active, _window, _cx| {
                            // Include active state in element ID to invalidate the cached
                            // tooltip when panel state changes (e.g., via keyboard shortcut)
                            let button = IconButton::new((name, is_active_button as u64), icon)
                                .icon_size(IconSize::Small)
                                .toggle_state(is_active_button)
                                .tab_index(0isize)
                                .aria_label(icon_tooltip)
                                .on_click({
                                    let action = action.boxed_clone();
                                    move |_, window, cx| {
                                        window.focus(&focus_handle, cx);
                                        window.dispatch_action(action.boxed_clone(), cx)
                                    }
                                })
                                .when(!is_active, |this| {
                                    this.tooltip(move |_window, cx| {
                                        Tooltip::for_action(tooltip.clone(), &*action, cx)
                                    })
                                });

                            div().relative().child(button).when_some(
                                icon_label
                                    .clone()
                                    .filter(|_| !is_active_button)
                                    .and_then(|label| label.parse::<usize>().ok()),
                                |this, count| this.child(CountBadge::new(count)),
                            )
                        }),
                )
            })
            .collect();

        if dock_position == DockPosition::Right {
            buttons.reverse();
        }

        let has_buttons = !buttons.is_empty();

        h_flex()
            .gap_1()
            .when(
                has_buttons
                    && (dock.position == DockPosition::Bottom
                        || dock.position == DockPosition::Right),
                |this| this.child(Divider::vertical().color(DividerColor::Border)),
            )
            .children(buttons)
            .when(has_buttons && dock.position == DockPosition::Left, |this| {
                this.child(Divider::vertical().color(DividerColor::Border))
            })
    }
}

impl StatusItemView for PanelButtons {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn crate::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Nothing to do, panel buttons don't depend on the active center item
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // Panel buttons are hidden on a per-panel basis through each panel
        // button's own context menu.
        None
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test {
    use super::*;
    use gpui::{App, Context, Window, actions, div};

    pub struct TestPanel {
        pub position: DockPosition,
        pub zoomed: bool,
        pub active: bool,
        pub focus_handle: FocusHandle,
        pub activation_focus_handle: Option<FocusHandle>,
        pub default_size: Pixels,
        pub flexible: bool,
        pub activation_priority: u32,
    }
    actions!(test_only, [ToggleTestPanel]);

    impl EventEmitter<PanelEvent> for TestPanel {}

    impl TestPanel {
        pub fn new(position: DockPosition, activation_priority: u32, cx: &mut App) -> Self {
            Self {
                position,
                zoomed: false,
                active: false,
                focus_handle: cx.focus_handle(),
                activation_focus_handle: None,
                default_size: px(300.),
                flexible: false,
                activation_priority,
            }
        }

        pub fn new_flexible(
            position: DockPosition,
            activation_priority: u32,
            cx: &mut App,
        ) -> Self {
            Self {
                flexible: true,
                ..Self::new(position, activation_priority, cx)
            }
        }

        pub fn new_with_activation_child(
            position: DockPosition,
            activation_priority: u32,
            cx: &mut App,
        ) -> Self {
            Self {
                activation_focus_handle: Some(cx.focus_handle()),
                ..Self::new(position, activation_priority, cx)
            }
        }
    }

    impl Render for TestPanel {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("test")
                .track_focus(&self.focus_handle(cx))
                .children(self.activation_focus_handle.iter().map(|focus_handle| {
                    div().id("test-activation-child").track_focus(focus_handle)
                }))
        }
    }

    impl Panel for TestPanel {
        fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
            self.activation_focus_handle
                .clone()
                .unwrap_or_else(|| self.focus_handle(cx))
        }

        fn persistent_name() -> &'static str {
            "TestPanel"
        }

        fn panel_key() -> &'static str {
            "TestPanel"
        }

        fn position(&self, _window: &Window, _: &App) -> super::DockPosition {
            self.position
        }

        fn position_is_valid(&self, _: super::DockPosition) -> bool {
            true
        }

        fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
            self.position = position;
            cx.update_global::<SettingsStore, _>(|_, _| {});
        }

        fn default_size(&self, _window: &Window, _: &App) -> Pixels {
            self.default_size
        }

        fn initial_size_state(&self, _window: &Window, _: &App) -> PanelSizeState {
            PanelSizeState {
                size: None,
                flex: None,
            }
        }

        fn supports_flexible_size(&self) -> bool {
            self.flexible
        }

        fn has_flexible_size(&self, _window: &Window, _: &App) -> bool {
            self.flexible
        }

        fn set_flexible_size(
            &mut self,
            flexible: bool,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            self.flexible = flexible;
        }

        fn icon(&self, _window: &Window, _: &App) -> Option<ui::IconName> {
            None
        }

        fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
            None
        }

        fn toggle_action(&self) -> Box<dyn Action> {
            ToggleTestPanel.boxed_clone()
        }

        fn is_zoomed(&self, _window: &Window, _: &App) -> bool {
            self.zoomed
        }

        fn set_zoomed(&mut self, zoomed: bool, _window: &mut Window, _cx: &mut Context<Self>) {
            self.zoomed = zoomed;
        }

        fn set_active(&mut self, active: bool, _window: &mut Window, _cx: &mut Context<Self>) {
            self.active = active;
        }

        fn activation_priority(&self) -> u32 {
            self.activation_priority
        }
    }

    impl Focusable for TestPanel {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }
}
