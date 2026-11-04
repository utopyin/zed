use std::{
    collections::BTreeMap,
    hash::{Hash as _, Hasher as _},
    ops::Range,
    sync::Arc,
};

use collections::{FxHasher, HashMap, HashSet};
use file_icons::FileIcons;
use gpui::{
    Context, Entity, EventEmitter, ScrollStrategy, SharedString, Stateful, Subscription,
    UniformListScrollHandle, Window, uniform_list,
};
use language::BufferId;
use multi_buffer::{MultiBuffer, MultiBufferSnapshot, PathKey};
use smallvec::SmallVec;
use ui::{Checkbox, DiffStat, IndentGuideColors, Tooltip, prelude::*};
use util::rel_path::RelPath;

use crate::{Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};

const TREE_WIDTH: Pixels = px(260.);
const TREE_INDENT: f32 = 12.;
const ROW_HEIGHT: Rems = rems(1.5);

pub(crate) enum DiffFileTreeEvent {
    FileCountChanged,
}

/// A sidebar listing every file in a diff multibuffer, used to jump between
/// files and to track which ones have been reviewed.
pub(crate) struct DiffFileTree {
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    files: Vec<DiffFile>,
    rows: Vec<Row>,
    /// File indices in tree order; directory rows refer to ranges of it.
    file_order: Vec<usize>,
    collapsed_directories: HashSet<SharedString>,
    read: HashMap<PathKey, u64>,
    active_buffer: Option<BufferId>,
    scroll_handle: UniformListScrollHandle,
    _subscriptions: [Subscription; 2],
}

struct DiffFile {
    path_key: PathKey,
    path: Arc<RelPath>,
    buffer_id: BufferId,
    change: FileChange,
    added_rows: u32,
    removed_rows: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileChange {
    Added,
    Modified,
    Deleted,
}

enum Row {
    Directory {
        path: SharedString,
        name: SharedString,
        depth: usize,
        files: Range<usize>,
    },
    File {
        file_ix: usize,
        depth: usize,
    },
}

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Directory { depth, .. } | Row::File { depth, .. } => *depth,
        }
    }
}

#[derive(Default)]
struct DirectoryNode {
    directories: BTreeMap<SharedString, DirectoryNode>,
    files: Vec<usize>,
}

impl EventEmitter<DiffFileTreeEvent> for DiffFileTree {}

impl DiffFileTree {
    pub(crate) fn new(
        multibuffer: Entity<MultiBuffer>,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer_subscription =
            cx.subscribe(&multibuffer, |this, _, event, cx| match event {
                multi_buffer::Event::BufferRangesUpdated { .. }
                | multi_buffer::Event::BuffersRemoved { .. }
                | multi_buffer::Event::BufferDiffChanged => this.refresh(cx),
                _ => {}
            });
        let editor_subscription = cx.subscribe(&editor, |this, _, event, cx| match event {
            EditorEvent::ScrollPositionChanged { .. }
            | EditorEvent::SelectionsChanged { .. }
            | EditorEvent::BufferFoldToggled { .. } => this.update_active_buffer(cx),
            _ => {}
        });
        let mut this = Self {
            multibuffer,
            editor,
            files: Vec::new(),
            rows: Vec::new(),
            file_order: Vec::new(),
            collapsed_directories: HashSet::default(),
            read: HashMap::default(),
            active_buffer: None,
            scroll_handle: UniformListScrollHandle::new(),
            _subscriptions: [multibuffer_subscription, editor_subscription],
        };
        this.refresh(cx);
        this
    }

    pub(crate) fn has_multiple_files(&self) -> bool {
        self.files.len() > 1
    }

    pub(crate) fn toggle_active_file_read(&mut self, cx: &mut Context<Self>) {
        let Some(file_ix) = self
            .active_buffer
            .and_then(|buffer_id| self.files.iter().position(|f| f.buffer_id == buffer_id))
        else {
            return;
        };
        let is_read = self.is_read(file_ix);
        self.set_read([file_ix], !is_read, cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let had_multiple_files = self.has_multiple_files();
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let multibuffer = self.multibuffer.read(cx);

        let mut files = snapshot
            .buffers_with_paths()
            .map(|(buffer, path_key)| {
                let buffer_id = buffer.remote_id();
                let path = multibuffer
                    .buffer(buffer_id)
                    .and_then(|buffer| buffer.read(cx).file().map(|file| file.path().clone()))
                    .unwrap_or_else(|| path_key.path.clone());
                let (change, added_rows, removed_rows) =
                    match snapshot.diff_for_buffer_id(buffer_id) {
                        Some(diff) => {
                            let (added, removed) = diff.changed_row_counts();
                            let change = if !diff.base_text_exists() {
                                FileChange::Added
                            } else if buffer.len() == 0 && diff.base_text().len() > 0 {
                                FileChange::Deleted
                            } else {
                                FileChange::Modified
                            };
                            (change, added, removed)
                        }
                        None => (FileChange::Modified, 0, 0),
                    };
                DiffFile {
                    path_key: path_key.clone(),
                    path,
                    buffer_id,
                    change,
                    added_rows,
                    removed_rows,
                }
            })
            .collect::<Vec<_>>();
        files.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.path_key.cmp(&b.path_key))
        });

        let changed_since_read = self
            .read
            .iter()
            .filter_map(|(path_key, fingerprint)| {
                let file = files.iter().find(|file| &file.path_key == path_key);
                let unchanged = file.is_some_and(|file| {
                    diff_fingerprint(&snapshot, file.buffer_id) == Some(*fingerprint)
                });
                (!unchanged).then(|| (path_key.clone(), file.map(|file| file.buffer_id)))
            })
            .collect::<Vec<_>>();
        if !changed_since_read.is_empty() {
            for (path_key, _) in &changed_since_read {
                self.read.remove(path_key);
            }
            self.editor.update(cx, |editor, cx| {
                for buffer_id in changed_since_read.into_iter().filter_map(|(_, id)| id) {
                    editor.unfold_buffer(buffer_id, cx);
                }
            });
        }

        self.files = files;
        self.rebuild_rows();
        self.update_active_buffer(cx);
        if had_multiple_files != self.has_multiple_files() {
            cx.emit(DiffFileTreeEvent::FileCountChanged);
        }
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        let mut root = DirectoryNode::default();
        for (file_ix, file) in self.files.iter().enumerate() {
            let components = file.path.components().collect::<Vec<_>>();
            let Some((_, directories)) = components.split_last() else {
                root.files.push(file_ix);
                continue;
            };
            let mut node = &mut root;
            for directory in directories {
                node = node
                    .directories
                    .entry(SharedString::from(directory.to_string()))
                    .or_default();
            }
            node.files.push(file_ix);
        }

        self.rows.clear();
        self.file_order.clear();
        push_rows(
            &root,
            "",
            0,
            &self.collapsed_directories,
            &mut self.file_order,
            &mut self.rows,
        );
    }

    fn update_active_buffer(&mut self, cx: &mut Context<Self>) {
        let active_buffer = self.editor.update(cx, |editor, cx| {
            let scroll_top = editor.scroll_position(cx).y;
            let display_snapshot = editor.display_snapshot(cx);
            display_snapshot
                .sticky_header_excerpt(scroll_top)
                .map(|header| header.excerpt.buffer_id())
                .or_else(|| {
                    display_snapshot
                        .buffer_snapshot()
                        .excerpts()
                        .next()
                        .map(|excerpt| excerpt.context.start.buffer_id)
                })
        });
        if active_buffer == self.active_buffer {
            return;
        }
        self.active_buffer = active_buffer;
        if let Some(row_ix) = self.rows.iter().position(|row| {
            matches!(row, Row::File { file_ix, .. } if Some(self.files[*file_ix].buffer_id) == active_buffer)
        }) {
            self.scroll_handle
                .scroll_to_item(row_ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn is_read(&self, file_ix: usize) -> bool {
        self.read.contains_key(&self.files[file_ix].path_key)
    }

    fn set_read(
        &mut self,
        file_ixs: impl IntoIterator<Item = usize>,
        read: bool,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let mut buffer_ids = Vec::new();
        for file_ix in file_ixs {
            let file = &self.files[file_ix];
            if read {
                let Some(fingerprint) = diff_fingerprint(&snapshot, file.buffer_id) else {
                    continue;
                };
                self.read.insert(file.path_key.clone(), fingerprint);
            } else {
                self.read.remove(&file.path_key);
            }
            buffer_ids.push(file.buffer_id);
        }
        self.editor.update(cx, |editor, cx| {
            if read {
                editor.fold_buffers(buffer_ids, cx);
            } else {
                for buffer_id in buffer_ids {
                    editor.unfold_buffer(buffer_id, cx);
                }
            }
        });
        cx.notify();
    }

    fn reveal_file(&mut self, file_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let buffer_id = self.files[file_ix].buffer_id;
        let multibuffer = self.multibuffer.read(cx);
        let Some(position) = multibuffer.location_for_path(&self.files[file_ix].path_key, cx)
        else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            if editor.is_buffer_folded(buffer_id, cx) {
                editor.set_scroll_anchor(
                    crate::scroll::ScrollAnchor {
                        anchor: position,
                        offset: gpui::Point::default(),
                    },
                    window,
                    cx,
                );
            } else {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::top_relative(0.)),
                    window,
                    cx,
                    |selections| selections.select_ranges([position..position]),
                );
            }
            window.focus(&editor.focus_handle, cx);
        });
    }

    fn toggle_directory(&mut self, path: SharedString, cx: &mut Context<Self>) {
        if !self.collapsed_directories.remove(&path) {
            self.collapsed_directories.insert(path);
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn render_row(&self, row_ix: usize, cx: &Context<Self>) -> AnyElement {
        match &self.rows[row_ix] {
            Row::Directory {
                path,
                name,
                depth,
                files,
            } => self.render_directory(row_ix, path, name, *depth, files.clone(), cx),
            Row::File { file_ix, depth } => self.render_file(row_ix, *file_ix, *depth, cx),
        }
    }

    fn render_directory(
        &self,
        row_ix: usize,
        path: &SharedString,
        name: &SharedString,
        depth: usize,
        files: Range<usize>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let expanded = !self.collapsed_directories.contains(path);
        let file_ixs = self.file_order[files].to_vec();
        let read_count = file_ixs.iter().filter(|ix| self.is_read(**ix)).count();
        let read_state = if read_count == 0 {
            ToggleState::Unselected
        } else if read_count == file_ixs.len() {
            ToggleState::Selected
        } else {
            ToggleState::Indeterminate
        };
        let (chevron, folder) = if expanded {
            (IconName::ChevronDown, IconName::FolderOpen)
        } else {
            (IconName::ChevronRight, IconName::Folder)
        };
        let this = cx.weak_entity();

        self.row_container(row_ix, depth, false, cx)
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(Icon::new(chevron).size(IconSize::Small).color(Color::Muted))
                    .child(Icon::new(folder).size(IconSize::Small).color(Color::Muted))
                    .child(Label::new(name.clone()).color(Color::Muted).truncate()),
            )
            .child(self.read_checkbox(
                SharedString::from(format!("diff-file-tree-dir-read-{row_ix}")),
                read_state,
                file_ixs,
                cx,
            ))
            .on_click({
                let path = path.clone();
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.toggle_directory(path.clone(), cx))
                        .ok();
                }
            })
            .into_any_element()
    }

    fn render_file(
        &self,
        row_ix: usize,
        file_ix: usize,
        depth: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let file = &self.files[file_ix];
        let is_read = self.is_read(file_ix);
        let is_active = self.active_buffer == Some(file.buffer_id);
        let name: SharedString = file
            .path
            .file_name()
            .unwrap_or(file.path.as_unix_str())
            .to_string()
            .into();
        let label_color = match (is_read, file.change) {
            (true, _) => Color::Muted,
            (false, FileChange::Added) => Color::VersionControlAdded,
            (false, FileChange::Modified) => Color::VersionControlModified,
            (false, FileChange::Deleted) => Color::Disabled,
        };
        let icon = FileIcons::get_icon(file.path.as_std_path(), cx)
            .map(Icon::from_path)
            .unwrap_or_else(|| Icon::new(IconName::File))
            .size(IconSize::Small)
            .color(Color::Muted);
        let read_state = if is_read {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let this = cx.weak_entity();

        self.row_container(row_ix, depth, is_active, cx)
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(div().size(IconSize::Small.rems()).flex_none())
                    .child(icon)
                    .child(
                        Label::new(name)
                            .color(label_color)
                            .when(file.change == FileChange::Deleted, Label::strikethrough)
                            .truncate(),
                    ),
            )
            .when(!is_read, |row| {
                row.child(DiffStat::new(
                    SharedString::from(format!("diff-file-tree-stat-{row_ix}")),
                    file.added_rows as usize,
                    file.removed_rows as usize,
                ))
            })
            .child(self.read_checkbox(
                SharedString::from(format!("diff-file-tree-read-{row_ix}")),
                read_state,
                vec![file_ix],
                cx,
            ))
            .tooltip(Tooltip::text(SharedString::from(
                file.path.as_unix_str().to_string(),
            )))
            .on_click(move |_, window, cx| {
                this.update(cx, |this, cx| this.reveal_file(file_ix, window, cx))
                    .ok();
            })
            .into_any_element()
    }

    fn row_container(
        &self,
        row_ix: usize,
        depth: usize,
        is_active: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let colors = cx.theme().colors();
        h_flex()
            .id(("diff-file-tree-row", row_ix))
            .h(ROW_HEIGHT)
            .w_full()
            .pl(px(8. + depth as f32 * TREE_INDENT))
            .pr_1p5()
            .gap_1p5()
            .cursor_pointer()
            .when(is_active, |row| row.bg(colors.ghost_element_selected))
            .hover(|row| row.bg(colors.ghost_element_hover))
            .active(|row| row.bg(colors.ghost_element_active))
    }

    fn read_checkbox(
        &self,
        id: SharedString,
        state: ToggleState,
        file_ixs: Vec<usize>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let this = cx.weak_entity();
        div().flex_none().occlude().child(
            Checkbox::new(id, state)
                .fill()
                .tooltip(Tooltip::text(if state == ToggleState::Selected {
                    "Mark as Unread"
                } else {
                    "Mark as Read"
                }))
                .on_click(move |_, _, cx| {
                    let read = state != ToggleState::Selected;
                    this.update(cx, |this, cx| {
                        this.set_read(file_ixs.iter().copied(), read, cx)
                    })
                    .ok();
                    cx.stop_propagation();
                }),
        )
    }

    #[cfg(test)]
    pub(crate) fn rows_for_test(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| match row {
                Row::Directory { name, depth, .. } => format!("{}{name}/", "  ".repeat(*depth)),
                Row::File { file_ix, depth } => {
                    let file = &self.files[*file_ix];
                    let read = if self.is_read(*file_ix) {
                        " (read)"
                    } else {
                        ""
                    };
                    format!(
                        "{}{}{read}",
                        "  ".repeat(*depth),
                        file.path.file_name().unwrap_or_default()
                    )
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn set_read_for_test(&mut self, path: &str, read: bool, cx: &mut Context<Self>) {
        let file_ix = self
            .files
            .iter()
            .position(|file| file.path.as_unix_str() == path)
            .expect("no file with that path");
        self.set_read([file_ix], read, cx);
    }

    fn visible_depths(&self, range: Range<usize>) -> SmallVec<[usize; 64]> {
        self.rows[range.start.min(self.rows.len())..range.end.min(self.rows.len())]
            .iter()
            .map(Row::depth)
            .collect()
    }
}

impl Render for DiffFileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let read_count = self.read.len();
        let file_count = self.files.len();

        v_flex()
            .id("diff-file-tree")
            .flex_none()
            .w(TREE_WIDTH)
            .h_full()
            .border_r_1()
            .border_color(colors.border_variant)
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .flex_none()
                    .h(ROW_HEIGHT)
                    .px_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        Label::new(format!("{file_count} files"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{read_count} of {file_count} read"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                uniform_list(
                    "diff-file-tree-rows",
                    self.rows.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        range.map(|row_ix| this.render_row(row_ix, cx)).collect()
                    }),
                )
                .with_decoration(
                    ui::indent_guides(px(TREE_INDENT), IndentGuideColors::panel(cx))
                        .with_left_offset(px(14.))
                        .with_compute_indents_fn(cx.entity(), |this, range, _, _| {
                            this.visible_depths(range)
                        }),
                )
                .flex_1()
                .track_scroll(&self.scroll_handle),
            )
    }
}

fn push_rows(
    node: &DirectoryNode,
    parent_path: &str,
    depth: usize,
    collapsed_directories: &HashSet<SharedString>,
    file_order: &mut Vec<usize>,
    rows: &mut Vec<Row>,
) {
    for (name, mut child) in &node.directories {
        let mut name = name.to_string();
        while child.files.is_empty() && child.directories.len() == 1 {
            let (next_name, next_child) = child.directories.iter().next().unwrap();
            name = format!("{name}/{next_name}");
            child = next_child;
        }
        let path = SharedString::from(if parent_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_path}/{name}")
        });
        let row_ix = rows.len();
        rows.push(Row::Directory {
            path: path.clone(),
            name: name.into(),
            depth,
            files: 0..0,
        });
        let first_file = file_order.len();
        if collapsed_directories.contains(&path) {
            push_files(child, file_order);
        } else {
            push_rows(
                child,
                &path,
                depth + 1,
                collapsed_directories,
                file_order,
                rows,
            );
        }
        if let Row::Directory { files, .. } = &mut rows[row_ix] {
            *files = first_file..file_order.len();
        }
    }
    for file_ix in &node.files {
        rows.push(Row::File {
            file_ix: *file_ix,
            depth,
        });
        file_order.push(*file_ix);
    }
}

fn push_files(node: &DirectoryNode, file_order: &mut Vec<usize>) {
    for child in node.directories.values() {
        push_files(child, file_order);
    }
    file_order.extend(&node.files);
}

fn diff_fingerprint(snapshot: &MultiBufferSnapshot, buffer_id: BufferId) -> Option<u64> {
    let buffer = snapshot.buffer_for_id(buffer_id)?;
    let diff = snapshot.diff_for_buffer_id(buffer_id)?;
    let base_text = diff.base_text();
    let mut hasher = FxHasher::default();
    diff.base_text_exists().hash(&mut hasher);
    for hunk in diff.hunks(buffer) {
        for chunk in base_text.text_for_range(hunk.diff_base_byte_range.clone()) {
            chunk.hash(&mut hasher);
        }
        0xffu8.hash(&mut hasher);
        for chunk in buffer.text_for_range(hunk.buffer_range.clone()) {
            chunk.hash(&mut hasher);
        }
        0xfeu8.hash(&mut hasher);
    }
    Some(hasher.finish())
}
