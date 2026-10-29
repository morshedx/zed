use std::{ops::Range, sync::Arc};

use anyhow::Result;
use collections::HashSet;
use editor::{
    Anchor, Editor, EditorSettings, MultiBufferSnapshot, SelectionEffects, ToPoint as _,
    scroll::Autoscroll,
};
use file_icons::FileIcons;
use fs::Fs;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, KeyContext, ScrollStrategy, Subscription, UniformListScrollHandle, WeakEntity,
    Window, actions, px, uniform_list,
};
use multi_buffer::MultiBufferRow;
use settings::{ProjectSearchDock, Settings, SettingsStore};
use text::{BufferId, Point};
use ui::{HighlightedLabel, IconButtonShape, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathStyle};
use workspace::{
    ToolbarItemView as _, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    searchable::Direction,
};

use crate::{
    SelectNextMatch, SelectPreviousMatch, ToggleCaseSensitive, ToggleIncludeIgnored, ToggleRegex,
    ToggleWholeWord,
    project_search::{
        ActiveSettings, ProjectSearch, ProjectSearchBar, ProjectSearchView, ToggleAllSearchResults,
    },
};

const SEARCH_PANEL_KEY: &str = "SearchPanel";
/// How much of a line a result shows before its match, so that the match stays visible.
const CONTEXT_BEFORE_MATCH: usize = 24;
/// The longest text a result shows, counted from the start of its match.
const MAX_RESULT_TEXT: usize = 160;

actions!(
    search_panel,
    [
        /// Toggles the search panel.
        Toggle,
        /// Toggles focus on the search panel.
        ToggleFocus,
        /// Opens the search panel's results as a project search tab in the center pane.
        OpenInEditor
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            if is_enabled(cx) {
                workspace.toggle_panel_focus::<SearchPanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if is_enabled(cx) && !workspace.toggle_panel_focus::<SearchPanel>(window, cx) {
                workspace.close_panel::<SearchPanel>(window, cx);
            }
        });
    })
    .detach();
}

/// Whether project search opens in the search panel instead of the center pane.
pub(crate) fn is_enabled(cx: &App) -> bool {
    EditorSettings::get_global(cx).search.dock != ProjectSearchDock::Center
}

/// A dock panel for project search in the style of VS Code: the search inputs, and the results
/// as a list of files with one line for each match.
pub struct SearchPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    results_focus_handle: FocusHandle,
    /// Runs the searches and keeps the results. It is not shown: the panel lists its results.
    search_view: Entity<ProjectSearchView>,
    search_bar: Entity<ProjectSearchBar>,
    rows: Vec<ResultRow>,
    rows_source: Option<RowsSource>,
    file_count: usize,
    collapsed_files: HashSet<BufferId>,
    selected_row: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    zoomed: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Debug)]
enum ResultRow {
    File(FileRow),
    Match(MatchRow),
}

#[derive(Clone, Debug)]
struct FileRow {
    buffer_id: BufferId,
    name: SharedString,
    directory: Option<SharedString>,
    icon: Option<SharedString>,
    match_count: usize,
}

#[derive(Clone, Debug)]
struct MatchRow {
    match_index: usize,
    text: SharedString,
    highlight: Range<usize>,
}

/// What the rows were built from, so that they are built again only when it changes.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RowsSource {
    search_id: usize,
    match_count: usize,
    edit_count: usize,
    collapsed_count: usize,
}

impl SearchPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    pub(crate) fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let weak_workspace = workspace.weak_handle();
        let project = workspace.project().clone();
        let results_focus_handle = cx.focus_handle();
        let settings = cx
            .global::<ActiveSettings>()
            .0
            .get(&project.downgrade())
            .cloned();
        let model = cx.new(|cx| ProjectSearch::new(project, weak_workspace.clone(), cx));
        let search_view = cx.new(|cx| {
            let mut search_view =
                ProjectSearchView::new(weak_workspace.clone(), model, window, cx, settings);
            search_view.set_panel_results_focus(results_focus_handle.clone());
            search_view
        });
        let search_bar = cx.new(|cx| {
            let mut search_bar = ProjectSearchBar::new_in_panel();
            search_bar.set_active_pane_item(Some(&search_view), window, cx);
            search_bar
        });

        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.observe(&search_view, |this, _, cx| {
                this.update_rows(cx);
                cx.notify();
            }),
            cx.on_focus(&focus_handle, window, |this, window, cx| {
                let query_focus_handle = this.search_view.read(cx).query_editor_focus_handle(cx);
                window.focus(&query_focus_handle, cx);
            }),
            cx.on_focus(&results_focus_handle, window, |this, _, cx| {
                this.search_view
                    .update(cx, |search_view, cx| search_view.confirm_active_search(cx));
                if this.selected_row.is_none() && !this.rows.is_empty() {
                    this.selected_row = Some(0);
                    cx.notify();
                }
            }),
            cx.observe_global_in::<SettingsStore>(window, |_, _, cx| {
                if !is_enabled(cx) {
                    cx.emit(PanelEvent::Close);
                }
            }),
        ];

        Self {
            workspace: weak_workspace,
            fs: workspace.app_state().fs.clone(),
            focus_handle,
            results_focus_handle,
            search_view,
            search_bar,
            rows: Vec::new(),
            rows_source: None,
            file_count: 0,
            collapsed_files: HashSet::default(),
            selected_row: None,
            scroll_handle: UniformListScrollHandle::new(),
            zoomed: false,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn search_view(&self) -> &Entity<ProjectSearchView> {
        &self.search_view
    }

    fn update_rows(&mut self, cx: &mut Context<Self>) {
        let model = self.search_view.read(cx).entity.read(cx);
        let snapshot = model.excerpts.read(cx).snapshot(cx);
        let source = RowsSource {
            search_id: model.search_id,
            match_count: model.match_ranges.len(),
            edit_count: snapshot.edit_count(),
            collapsed_count: self.collapsed_files.len(),
        };
        if self.rows_source == Some(source) {
            return;
        }
        if self
            .rows_source
            .is_some_and(|previous| previous.search_id != source.search_id)
        {
            self.collapsed_files.clear();
            self.selected_row = None;
        }

        let mut rows = Vec::new();
        let mut file_count = 0;
        let mut current_file_row = None;
        for (match_index, match_range) in model.match_ranges.iter().enumerate() {
            let Some((buffer_snapshot, _)) =
                snapshot.anchor_range_to_buffer_anchor_range(match_range.clone())
            else {
                continue;
            };
            let buffer_id = buffer_snapshot.remote_id();
            let in_current_file = current_file_row
                .and_then(|index| match rows.get(index) {
                    Some(ResultRow::File(file)) => Some(file.buffer_id == buffer_id),
                    _ => None,
                })
                .unwrap_or(false);
            if !in_current_file {
                let path = buffer_snapshot.file().map(|file| file.path().clone());
                let name = path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .unwrap_or("untitled")
                    .to_string();
                let directory = path
                    .as_ref()
                    .and_then(|path| path.parent())
                    .filter(|directory| !directory.is_empty())
                    .map(|directory| directory.display(PathStyle::local()).to_string().into());
                let icon = path
                    .as_ref()
                    .and_then(|path| FileIcons::get_icon(path.as_std_path(), cx));
                current_file_row = Some(rows.len());
                file_count += 1;
                rows.push(ResultRow::File(FileRow {
                    buffer_id,
                    name: name.into(),
                    directory,
                    icon,
                    match_count: 0,
                }));
            }
            if let Some(ResultRow::File(file)) =
                current_file_row.and_then(|index| rows.get_mut(index))
            {
                file.match_count += 1;
            }
            if !self.collapsed_files.contains(&buffer_id) {
                let (text, highlight) = match_text(match_range, &snapshot);
                rows.push(ResultRow::Match(MatchRow {
                    match_index,
                    text,
                    highlight,
                }));
            }
        }

        self.rows = rows;
        self.file_count = file_count;
        self.rows_source = Some(source);
        if self.selected_row.is_some_and(|row| row >= self.rows.len()) {
            self.selected_row = self.rows.len().checked_sub(1);
        }
    }

    fn row_for_match(&self, match_index: usize) -> Option<usize> {
        self.rows.iter().position(|row| match row {
            ResultRow::Match(row) => row.match_index == match_index,
            ResultRow::File(_) => false,
        })
    }

    fn file_row_for(&self, row_index: usize) -> Option<usize> {
        self.rows
            .get(..=row_index)?
            .iter()
            .rposition(|row| matches!(row, ResultRow::File(_)))
    }

    fn select_row(&mut self, row_index: usize, cx: &mut Context<Self>) {
        self.selected_row = Some(row_index);
        self.scroll_handle
            .scroll_to_item(row_index, ScrollStrategy::Center);
        cx.notify();
    }

    fn toggle_file(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        if !self.collapsed_files.remove(&buffer_id) {
            self.collapsed_files.insert(buffer_id);
        }
        self.update_rows(cx);
        if let Some(file_row) = self.rows.iter().position(|row| match row {
            ResultRow::File(file) => file.buffer_id == buffer_id,
            ResultRow::Match(_) => false,
        }) {
            self.select_row(file_row, cx);
        }
        cx.notify();
    }

    fn toggle_all_files(
        &mut self,
        _: &ToggleAllSearchResults,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let file_ids = self
            .rows
            .iter()
            .filter_map(|row| match row {
                ResultRow::File(file) => Some(file.buffer_id),
                ResultRow::Match(_) => None,
            })
            .collect::<Vec<_>>();
        if self.collapsed_files.is_empty() {
            self.collapsed_files.extend(file_ids);
        } else {
            self.collapsed_files.clear();
        }
        self.selected_row = None;
        self.update_rows(cx);
        cx.notify();
    }

    /// Opens the file of a match in the center pane and selects the match. Without
    /// `focus_editor`, the focus stays in the panel, so that the user can go on through the list.
    fn open_match(
        &mut self,
        match_index: usize,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_view.update(cx, |search_view, cx| {
            search_view.select_match_at(match_index, window, cx)
        });
        let model = self.search_view.read(cx).entity.read(cx);
        let Some(match_range) = model.match_ranges.get(match_index).cloned() else {
            return;
        };
        let excerpts = model.excerpts.read(cx);
        let snapshot = excerpts.snapshot(cx);
        let Some((buffer_snapshot, buffer_range)) =
            snapshot.anchor_range_to_buffer_anchor_range(match_range)
        else {
            return;
        };
        let Some(buffer) = excerpts.buffer(buffer_snapshot.remote_id()) else {
            return;
        };
        let point_range = text::ToPoint::to_point(&buffer_range.start, buffer_snapshot)
            ..text::ToPoint::to_point(&buffer_range.end, buffer_snapshot);

        let workspace = self.workspace.clone();
        // Opening the file changes the center pane, which must not happen while this panel is
        // being updated.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    let editor = workspace.open_project_item::<Editor>(
                        None,
                        buffer,
                        focus_editor,
                        focus_editor,
                        false,
                        true,
                        window,
                        cx,
                    );
                    editor.update(cx, |editor, cx| {
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges([point_range]),
                        );
                    });
                })
                .log_err();
        });
    }

    fn navigate(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        self.search_view.update(cx, |search_view, cx| {
            search_view.select_match(direction, window, cx)
        });
        let Some(match_index) = self.search_view.read(cx).active_match_index() else {
            return;
        };
        let model = self.search_view.read(cx).entity.read(cx);
        let snapshot = model.excerpts.read(cx).snapshot(cx);
        if let Some(buffer_id) = model.match_ranges.get(match_index).and_then(|range| {
            snapshot
                .anchor_range_to_buffer_anchor_range(range.clone())
                .map(|(buffer, _)| buffer.remote_id())
        }) && self.collapsed_files.remove(&buffer_id)
        {
            self.update_rows(cx);
        }
        if let Some(row_index) = self.row_for_match(match_index) {
            self.select_row(row_index, cx);
        }
        self.open_match(match_index, false, window, cx);
    }

    fn select_next_match(
        &mut self,
        _: &SelectNextMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.navigate(Direction::Next, window, cx);
    }

    fn select_previous_match(
        &mut self,
        _: &SelectPreviousMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.navigate(Direction::Prev, window, cx);
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.selected_row.map_or(0, |row| row + 1);
        if next < self.rows.len() {
            self.select_row(next, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.selected_row.and_then(|row| row.checked_sub(1)) {
            self.select_row(previous, cx);
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.select_row(0, cx);
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.rows.len().checked_sub(1) {
            self.select_row(last, cx);
        }
    }

    fn expand_selected_file(
        &mut self,
        _: &menu::SelectChild,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ResultRow::File(file)) = self.selected_row.and_then(|row| self.rows.get(row))
            && self.collapsed_files.contains(&file.buffer_id)
        {
            self.toggle_file(file.buffer_id, cx);
        }
    }

    fn collapse_selected_file(
        &mut self,
        _: &menu::SelectParent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_row) = self.selected_row else {
            return;
        };
        match self.rows.get(selected_row) {
            Some(ResultRow::File(file)) if !self.collapsed_files.contains(&file.buffer_id) => {
                self.toggle_file(file.buffer_id, cx);
            }
            Some(ResultRow::Match(_)) => {
                if let Some(file_row) = self.file_row_for(selected_row) {
                    self.select_row(file_row, cx);
                }
            }
            _ => {}
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match self
            .selected_row
            .and_then(|row| self.rows.get(row))
            .cloned()
        {
            Some(ResultRow::File(file)) => self.toggle_file(file.buffer_id, cx),
            Some(ResultRow::Match(row)) => self.open_match(row.match_index, true, window, cx),
            None => {}
        }
    }

    fn open_in_editor(&mut self, _: &OpenInEditor, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.search_view.read(cx).entity.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let weak_workspace = self.workspace.clone();
        let model = model.update(cx, |model, cx| model.clone(cx));
        workspace.update(cx, |workspace, cx| {
            let search_view =
                cx.new(|cx| ProjectSearchView::new(weak_workspace, model, window, cx, None));
            workspace.add_item_to_active_pane(Box::new(search_view), None, true, window, cx);
        });
    }

    fn toggle_search_option(
        &mut self,
        option: crate::SearchOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_bar.update(cx, |search_bar, cx| {
            search_bar.toggle_search_option(option, window, cx);
        });
    }

    fn render_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let search_view = self.search_view.read(cx);
        let match_count = search_view.entity.read(cx).match_ranges.len();
        let summary = if match_count == 0 {
            search_view.status_text(cx).map(SharedString::from)
        } else {
            let more = if search_view.search_limit_reached(cx) {
                "+"
            } else {
                ""
            };
            let results = if match_count == 1 {
                "result"
            } else {
                "results"
            };
            let files = if self.file_count == 1 {
                "file"
            } else {
                "files"
            };
            Some(
                format!(
                    "{match_count}{more} {results} in {} {files}",
                    self.file_count
                )
                .into(),
            )
        };
        let all_collapsed = !self.collapsed_files.is_empty();

        h_flex()
            .w_full()
            .px_2()
            .gap_1()
            .justify_between()
            .child(
                Label::new(summary.unwrap_or_default())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .when(match_count > 0, |this| {
                this.child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            IconButton::new(
                                "search-panel-toggle-all",
                                if all_collapsed {
                                    IconName::ChevronUpDown
                                } else {
                                    IconName::ChevronDownUp
                                },
                            )
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(if all_collapsed {
                                "Expand All"
                            } else {
                                "Collapse All"
                            }))
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.toggle_all_files(&ToggleAllSearchResults, window, cx);
                                },
                            )),
                        )
                        .child(
                            IconButton::new("search-panel-open-in-editor", IconName::ArrowUpRight)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::Small)
                                .tooltip(|_, cx| {
                                    Tooltip::for_action("Open in Editor", &OpenInEditor, cx)
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_in_editor(&OpenInEditor, window, cx);
                                })),
                        ),
                )
            })
    }

    fn render_row(&self, row_index: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(row_index) else {
            return div().into_any_element();
        };
        let selected = self.selected_row == Some(row_index);
        let focused = selected && self.results_focus_handle.contains_focused(window, cx);
        match row {
            ResultRow::File(file) => {
                let buffer_id = file.buffer_id;
                let expanded = !self.collapsed_files.contains(&buffer_id);
                ListItem::new(("search-panel-file", row_index))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle(expanded)
                    .toggle_state(selected)
                    .focused(focused)
                    .on_toggle(cx.listener(move |this, _, _, cx| this.toggle_file(buffer_id, cx)))
                    .start_slot(
                        file.icon
                            .clone()
                            .map(|icon| {
                                Icon::from_path(icon).color(Color::Muted).into_any_element()
                            })
                            .unwrap_or_else(|| {
                                Icon::new(IconName::File)
                                    .color(Color::Muted)
                                    .into_any_element()
                            }),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1p5()
                            .child(Label::new(file.name.clone()).truncate())
                            .children(file.directory.clone().map(|directory| {
                                Label::new(directory)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate()
                            })),
                    )
                    .end_slot(
                        Label::new(file.match_count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(buffer_id, cx)))
                    .into_any_element()
            }
            ResultRow::Match(row) => {
                let match_index = row.match_index;
                ListItem::new(("search-panel-match", row_index))
                    .spacing(ListItemSpacing::Dense)
                    .indent_level(1)
                    .indent_step_size(px(20.))
                    .toggle_state(selected)
                    .focused(focused)
                    .child(
                        HighlightedLabel::from_ranges(
                            row.text.clone(),
                            vec![row.highlight.clone()],
                        )
                        .size(LabelSize::Small)
                        .truncate(),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.select_row(row_index, cx);
                        this.open_match(match_index, event.click_count() > 1, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }
}

/// The line of a match, without its indentation, and the byte range of the match in it.
fn match_text(
    match_range: &Range<Anchor>,
    snapshot: &MultiBufferSnapshot,
) -> (SharedString, Range<usize>) {
    let start = match_range.start.to_point(snapshot);
    let end = match_range.end.to_point(snapshot);
    let line_end = Point::new(start.row, snapshot.line_len(MultiBufferRow(start.row)));
    let line = snapshot
        .text_for_range(Point::new(start.row, 0)..line_end)
        .collect::<String>();
    let match_start = (start.column as usize).min(line.len());
    let match_end = if end.row == start.row {
        (end.column as usize).clamp(match_start, line.len())
    } else {
        line.len()
    };

    let indentation = line.len() - line.trim_start().len();
    let mut text_start = indentation.min(match_start);
    let mut prefix = "";
    if match_start - text_start > CONTEXT_BEFORE_MATCH {
        text_start = line.ceil_char_boundary(match_start - CONTEXT_BEFORE_MATCH);
        prefix = "…";
    }
    let text_end = line.floor_char_boundary((match_start + MAX_RESULT_TEXT).min(line.len()));
    let text_end = text_end.max(match_start);
    let text = format!("{prefix}{}", &line[text_start..text_end]);
    let highlight_start = prefix.len() + match_start - text_start;
    let highlight_end = prefix.len() + match_end.min(text_end) - text_start;
    (text.into(), highlight_start..highlight_end)
}

impl EventEmitter<PanelEvent> for SearchPanel {}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut results_context = KeyContext::new_with_defaults(cx.platform_info().os);
        results_context.add("SearchPanelResults");
        results_context.add("menu");
        let has_rows = !self.rows.is_empty();

        v_flex()
            .id("search-panel")
            .key_context("SearchPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .gap_1()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &ToggleRegex, window, cx| {
                this.toggle_search_option(crate::SearchOptions::REGEX, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, window, cx| {
                this.toggle_search_option(crate::SearchOptions::CASE_SENSITIVE, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleWholeWord, window, cx| {
                this.toggle_search_option(crate::SearchOptions::WHOLE_WORD, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleIncludeIgnored, window, cx| {
                this.toggle_search_option(crate::SearchOptions::INCLUDE_IGNORED, window, cx)
            }))
            .on_action(cx.listener(Self::toggle_all_files))
            .on_action(cx.listener(Self::open_in_editor))
            .capture_action(cx.listener(Self::select_next_match))
            .capture_action(cx.listener(Self::select_previous_match))
            .child(div().pt_2().px_2().child(self.search_bar.clone()))
            .child(self.render_summary(cx))
            .child(
                div()
                    .id("search-panel-results")
                    .key_context(results_context)
                    .track_focus(&self.results_focus_handle)
                    .flex_1()
                    .min_h_0()
                    .on_action(cx.listener(Self::select_next))
                    .on_action(cx.listener(Self::select_previous))
                    .on_action(cx.listener(Self::select_first))
                    .on_action(cx.listener(Self::select_last))
                    .on_action(cx.listener(Self::expand_selected_file))
                    .on_action(cx.listener(Self::collapse_selected_file))
                    .on_action(cx.listener(Self::confirm))
                    .when(has_rows, |this| {
                        this.child(
                            uniform_list(
                                "search-panel-results-list",
                                self.rows.len(),
                                cx.processor(|this, range: Range<usize>, window, cx| {
                                    range.map(|row| this.render_row(row, window, cx)).collect()
                                }),
                            )
                            .size_full()
                            .track_scroll(&self.scroll_handle),
                        )
                    }),
            )
    }
}

impl Panel for SearchPanel {
    fn persistent_name() -> &'static str {
        "SearchPanel"
    }

    fn panel_key() -> &'static str {
        SEARCH_PANEL_KEY
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_view.read(cx).query_editor_focus_handle(cx)
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        match EditorSettings::get_global(cx).search.dock {
            ProjectSearchDock::Right => DockPosition::Right,
            ProjectSearchDock::Left | ProjectSearchDock::Center => DockPosition::Left,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => ProjectSearchDock::Left,
                DockPosition::Right => ProjectSearchDock::Right,
            };
            settings.editor.search.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        is_enabled(cx).then_some(IconName::MagnifyingGlass)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Search Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(Toggle)
    }

    fn is_zoomed(&self, _: &Window, _: &App) -> bool {
        self.zoomed
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = zoomed;
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        4
    }

    fn enabled(&self, cx: &App) -> bool {
        is_enabled(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReplaceAll, SearchOptions, ToggleReplace};
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path;
    use workspace::{DeploySearch, MultiWorkspace};

    fn init_test(dock: ProjectSearchDock, cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    let search = settings.editor.search.get_or_insert_default();
                    search.search_on_type = Some(false);
                    search.dock = Some(dock);
                });
            });
        });
    }

    async fn workspace_with_search_panel(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Workspace>,
        Entity<SearchPanel>,
        &mut VisualTestContext,
    ) {
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "one.rs": "const ONE: usize = 1;",
                "two.rs": "const TWO: usize = one::ONE + one::ONE;",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| SearchPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        (fs, workspace, panel, cx)
    }

    fn search_in_panel(query: &str, workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        workspace.update_in(cx, |workspace, window, cx| {
            ProjectSearchView::deploy_search(
                workspace,
                &DeploySearch {
                    query: Some(query.to_string()),
                    ..DeploySearch::default()
                },
                window,
                cx,
            )
        });
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
    }

    fn row_summaries(panel: &Entity<SearchPanel>, cx: &mut VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, _| {
            panel
                .rows
                .iter()
                .map(|row| match row {
                    ResultRow::File(file) => format!("{} ({})", file.name, file.match_count),
                    ResultRow::Match(row) => {
                        format!("  {} [{}]", row.text, &row.text[row.highlight.clone()])
                    }
                })
                .collect()
        })
    }

    #[gpui::test]
    async fn test_search_panel_lists_and_opens_results(cx: &mut TestAppContext) {
        init_test(ProjectSearchDock::Right, cx);
        let (_fs, workspace, panel, cx) = workspace_with_search_panel(cx).await;

        search_in_panel("ONE", &workspace, cx);
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.right_dock().read(cx).is_open());
            assert!(
                workspace.active_pane().read(cx).items().next().is_none(),
                "the center pane gets no search tab"
            );
        });
        // Case-insensitive: "ONE" also matches the two "one::" in two.rs. A match far into its
        // line drops the start of the line, so that it stays visible in the narrow panel.
        assert_eq!(
            row_summaries(&panel, cx),
            [
                "one.rs (1)",
                "  const ONE: usize = 1; [ONE]",
                "two.rs (4)",
                "  const TWO: usize = one::ONE + one::ONE; [one]",
                "  const TWO: usize = one::ONE + one::ONE; [ONE]",
                "  …TWO: usize = one::ONE + one::ONE; [one]",
                "  …usize = one::ONE + one::ONE; [ONE]",
            ]
        );
        panel.update_in(cx, |panel, window, cx| {
            let search_view = panel.search_view.read(cx);
            assert!(
                search_view.query_editor_focus_handle(cx).is_focused(window),
                "as in VS Code, the focus stays in the query"
            );
        });

        // The panel handles the search option keys, which the workspace would send to the
        // center pane's search bar.
        cx.dispatch_action(ToggleRegex);
        cx.run_until_parked();
        panel.read_with(cx, |panel, cx| {
            assert!(
                panel
                    .search_view
                    .read(cx)
                    .search_options
                    .contains(SearchOptions::REGEX)
            );
        });

        // A match opens its file in the center pane, with the match selected.
        panel.update_in(cx, |panel, window, cx| {
            panel.open_match(2, true, window, cx)
        });
        cx.run_until_parked();
        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<Editor>())
                .expect("the match opens in an editor")
        });
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.title(cx), "two.rs");
            let selection = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx));
            assert_eq!(selection.range(), Point::new(0, 24)..Point::new(0, 27));
        });

        // Collapsing a file hides its matches.
        let two_rs = panel.read_with(cx, |panel, _| match &panel.rows[2] {
            ResultRow::File(file) => file.buffer_id,
            ResultRow::Match(_) => panic!("expected a file row"),
        });
        panel.update(cx, |panel, cx| panel.toggle_file(two_rs, cx));
        assert_eq!(
            row_summaries(&panel, cx),
            ["one.rs (1)", "  const ONE: usize = 1; [ONE]", "two.rs (4)"]
        );
    }

    #[gpui::test]
    async fn test_search_stays_in_center_by_default(cx: &mut TestAppContext) {
        init_test(ProjectSearchDock::Center, cx);
        let (_fs, workspace, panel, cx) = workspace_with_search_panel(cx).await;

        workspace.update_in(cx, |workspace, window, cx| {
            ProjectSearchView::deploy_search(workspace, &DeploySearch::default(), window, cx)
        });
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace
                    .active_item(cx)
                    .and_then(|item| item.downcast::<ProjectSearchView>())
                    .is_some()
            );
            assert!(!workspace.left_dock().read(cx).is_open());
            assert!(!workspace.right_dock().read(cx).is_open());
        });
        assert!(row_summaries(&panel, cx).is_empty());
    }

    #[gpui::test]
    async fn test_replace_all_in_panel_saves_files(cx: &mut TestAppContext) {
        init_test(ProjectSearchDock::Left, cx);
        let (fs, workspace, _panel, cx) = workspace_with_search_panel(cx).await;

        search_in_panel("TWO", &workspace, cx);
        cx.dispatch_action(ToggleReplace);
        cx.simulate_input("DOS");
        cx.dispatch_action(ReplaceAll);
        cx.run_until_parked();
        assert_eq!(
            fs.load(path!("/dir/two.rs").as_ref()).await.unwrap(),
            "const DOS: usize = one::ONE + one::ONE;"
        );
    }
}
