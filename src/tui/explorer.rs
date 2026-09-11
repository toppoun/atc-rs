use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirEntry};
use std::io;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::view::clip_text_with_ellipsis;

const HEADER: &str = "Explorer";
const INDENT_WIDTH: usize = 2;
const COLLAPSED_GLYPH: char = '▶';
const EXPANDED_GLYPH: char = '▼';

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeNode {
    name: OsString,
    expanded: bool,
    /// `None` is unloaded; `Some`, including an empty vector, is loaded.
    children: Option<Vec<PathBuf>>,
}

impl TreeNode {
    fn unloaded(name: OsString) -> Self {
        Self {
            name,
            expanded: false,
            children: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoadedChild {
    path: PathBuf,
    name: OsString,
}

impl LoadedChild {
    #[cfg(test)]
    fn directory(path: PathBuf) -> Self {
        let name = display_name(&path);
        Self { path, name }
    }
}

/// One row in the currently visible, in-memory tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibleRow<'a> {
    path: &'a Path,
    name: &'a OsStr,
    depth: usize,
    expanded: bool,
}

/// Reusable directory-tree state with exact lexical path identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplorerState {
    root: PathBuf,
    selected: PathBuf,
    nodes: HashMap<PathBuf, TreeNode>,
    scroll: usize,
    status: Option<String>,
}

impl ExplorerState {
    /// Creates a collapsed, unloaded root without performing filesystem I/O.
    pub(crate) fn new(root: PathBuf) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(root.clone(), TreeNode::unloaded(display_name(&root)));
        Self {
            selected: root.clone(),
            root,
            nodes,
            scroll: 0,
            status: None,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn selected_path(&self) -> &Path {
        &self.selected
    }

    pub(crate) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    #[cfg(test)]
    fn scroll(&self) -> usize {
        self.scroll
    }

    /// Flattens only cached, expanded nodes. This method never touches the filesystem.
    fn visible_rows(&self) -> Vec<VisibleRow<'_>> {
        let mut rows = Vec::new();
        let mut stack = vec![(self.root.as_path(), 0usize)];

        while let Some((path, depth)) = stack.pop() {
            let Some(node) = self.nodes.get(path) else {
                continue;
            };
            rows.push(VisibleRow {
                path,
                name: &node.name,
                depth,
                expanded: node.expanded,
            });

            if node.expanded
                && let Some(children) = node.children.as_ref()
            {
                for child in children.iter().rev() {
                    stack.push((child.as_path(), depth.saturating_add(1)));
                }
            }
        }

        rows
    }

    pub(crate) fn select_next(&mut self) -> bool {
        let rows = self.visible_rows();
        let Some(index) = rows.iter().position(|row| row.path == self.selected) else {
            return false;
        };
        let Some(next) = rows
            .get(index.saturating_add(1))
            .map(|row| row.path.to_path_buf())
        else {
            return false;
        };
        self.selected = next;
        true
    }

    pub(crate) fn select_previous(&mut self) -> bool {
        let rows = self.visible_rows();
        let Some(index) = rows.iter().position(|row| row.path == self.selected) else {
            return false;
        };
        let Some(previous) = index.checked_sub(1).and_then(|index| rows.get(index)) else {
            return false;
        };
        self.selected = previous.path.to_path_buf();
        true
    }

    pub(crate) fn toggle_selected(&mut self) -> bool {
        if self.selected_node().is_some_and(|node| node.expanded) {
            self.collapse_selected()
        } else {
            self.expand_selected()
        }
    }

    fn expand_selected(&mut self) -> bool {
        self.expand_selected_with(load_directory)
    }

    fn collapse_selected(&mut self) -> bool {
        let Some(node) = self.nodes.get_mut(&self.selected) else {
            return false;
        };
        if !node.expanded {
            return false;
        }
        node.expanded = false;
        self.reconcile_after_tree_change();
        true
    }

    /// Right/l: expand a collapsed node, otherwise select its first child.
    pub(crate) fn select_right(&mut self) -> bool {
        self.select_right_with(load_directory)
    }

    fn select_right_with<L>(&mut self, loader: L) -> bool
    where
        L: FnMut(&Path) -> io::Result<Vec<LoadedChild>>,
    {
        let Some(node) = self.selected_node() else {
            return false;
        };
        if !node.expanded {
            return self.expand_selected_with(loader);
        }
        let Some(first_child) = node
            .children
            .as_ref()
            .and_then(|children| children.first())
            .cloned()
        else {
            return false;
        };
        self.selected = first_child;
        true
    }

    /// Left/h: collapse an expanded node, otherwise select its visible parent.
    pub(crate) fn select_left(&mut self) -> bool {
        if self.selected_node().is_some_and(|node| node.expanded) {
            return self.collapse_selected();
        }

        let rows = self.visible_rows();
        let Some(index) = rows.iter().position(|row| row.path == self.selected) else {
            return false;
        };
        let Some(parent_depth) = rows[index].depth.checked_sub(1) else {
            return false;
        };
        let Some(parent) = rows[..index]
            .iter()
            .rev()
            .find(|row| row.depth == parent_depth)
            .map(|row| row.path.to_path_buf())
        else {
            return false;
        };
        self.selected = parent;
        true
    }

    /// Reloads only the selected directory. Its expanded state is retained.
    pub(crate) fn reload_selected(&mut self) -> bool {
        self.reload_selected_with(load_directory)
    }

    /// Rebases to the lexical parent without loading it.
    pub(crate) fn rebase_to_parent(&mut self) -> bool {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else {
            return false;
        };
        self.replace_root(parent);
        true
    }

    /// Rebases to an arbitrary existing directory without canonicalizing or loading it.
    /// On failure, all tree state is retained and only the generic status changes.
    pub(crate) fn rebase(&mut self, root: PathBuf) -> bool {
        match fs::metadata(&root) {
            Ok(metadata) if metadata.is_dir() => {
                self.replace_root(root);
                true
            }
            Ok(_) => {
                let error = io::Error::new(io::ErrorKind::InvalidInput, "path is not a directory");
                self.set_io_status("Could not rebase to", &root, &error);
                false
            }
            Err(error) => {
                self.set_io_status("Could not rebase to", &root, &error);
                false
            }
        }
    }

    /// Keeps the selected visible row inside a viewport of `height` rows.
    fn reconcile_scroll(&mut self, height: usize) {
        if height == 0 {
            self.scroll = 0;
            return;
        }

        self.reconcile_selection();
        let (row_count, selected_index) = {
            let rows = self.visible_rows();
            (
                rows.len(),
                rows.iter()
                    .position(|row| row.path == self.selected)
                    .unwrap_or(0),
            )
        };

        if selected_index < self.scroll {
            self.scroll = selected_index;
        } else if selected_index >= self.scroll.saturating_add(height) {
            self.scroll = selected_index.saturating_add(1).saturating_sub(height);
        }

        self.scroll = self.scroll.min(row_count.saturating_sub(height));
    }

    fn selected_node(&self) -> Option<&TreeNode> {
        self.nodes.get(&self.selected)
    }

    fn expand_selected_with<L>(&mut self, mut loader: L) -> bool
    where
        L: FnMut(&Path) -> io::Result<Vec<LoadedChild>>,
    {
        let selected = self.selected.clone();
        let Some(node) = self.nodes.get(&selected) else {
            return false;
        };
        if node.expanded {
            return false;
        }

        if node.children.is_none() {
            let children = match loader(&selected) {
                Ok(children) => children,
                Err(error) => {
                    self.set_io_status("Could not load", &selected, &error);
                    return false;
                }
            };
            self.commit_children(&selected, children);
        }

        if let Some(node) = self.nodes.get_mut(&selected) {
            node.expanded = true;
        }
        self.status = None;
        self.reconcile_after_tree_change();
        true
    }

    fn reload_selected_with<L>(&mut self, mut loader: L) -> bool
    where
        L: FnMut(&Path) -> io::Result<Vec<LoadedChild>>,
    {
        let selected = self.selected.clone();
        if !self.nodes.contains_key(&selected) {
            return false;
        }
        let children = match loader(&selected) {
            Ok(children) => children,
            Err(error) => {
                self.set_io_status("Could not reload", &selected, &error);
                return false;
            }
        };

        self.commit_children(&selected, children);
        self.status = None;
        self.reconcile_after_tree_change();
        true
    }

    fn commit_children(&mut self, parent: &Path, mut loaded: Vec<LoadedChild>) {
        sort_children(&mut loaded);
        let old_children = self
            .nodes
            .get(parent)
            .and_then(|node| node.children.clone())
            .unwrap_or_default();
        let new_paths: Vec<PathBuf> = loaded.iter().map(|child| child.path.clone()).collect();
        let new_path_set: HashSet<&Path> = new_paths.iter().map(PathBuf::as_path).collect();

        for removed in old_children
            .iter()
            .filter(|path| !new_path_set.contains(path.as_path()))
        {
            self.remove_subtree(removed);
        }

        for child in loaded {
            self.nodes
                .entry(child.path)
                .and_modify(|node| node.name = child.name.clone())
                .or_insert_with(|| TreeNode::unloaded(child.name));
        }

        if let Some(node) = self.nodes.get_mut(parent) {
            node.children = Some(new_paths);
        }
    }

    fn remove_subtree(&mut self, root: &Path) {
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            if let Some(node) = self.nodes.remove(&path)
                && let Some(children) = node.children
            {
                stack.extend(children);
            }
        }
    }

    fn replace_root(&mut self, root: PathBuf) {
        self.root = root.clone();
        self.selected = root.clone();
        self.nodes.clear();
        self.nodes
            .insert(root.clone(), TreeNode::unloaded(display_name(&root)));
        self.scroll = 0;
        self.status = None;
    }

    fn reconcile_after_tree_change(&mut self) {
        self.reconcile_selection();
        let row_count = self.visible_rows().len();
        self.scroll = self.scroll.min(row_count.saturating_sub(1));
    }

    fn reconcile_selection(&mut self) {
        let replacement = {
            let visible: HashSet<&Path> = self
                .visible_rows()
                .into_iter()
                .map(|row| row.path)
                .collect();
            if visible.contains(self.selected.as_path()) {
                None
            } else {
                let mut candidate = self.selected.as_path();
                Some(loop {
                    let Some(parent) = candidate.parent() else {
                        break self.root.clone();
                    };
                    if visible.contains(parent) {
                        break parent.to_path_buf();
                    }
                    candidate = parent;
                })
            }
        };

        if let Some(replacement) = replacement {
            self.selected = replacement;
        }
    }

    fn set_io_status(&mut self, action: &str, path: &Path, error: &io::Error) {
        self.status = Some(format!("{action} {}: {error}", path.display()));
    }
}

/// Draws the Explorer into a standalone pane. The right separator is never part of a row style.
pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &mut ExplorerState) {
    if area.width == 0 || area.height == 0 {
        state.reconcile_scroll(0);
        return;
    }

    frame.render_widget(Block::default().borders(Borders::RIGHT), area);
    let usable_width = area.width.saturating_sub(1);
    let header_area = Rect::new(area.x, area.y, usable_width, area.height.min(1));
    if header_area.width > 0 && header_area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                clip_text_with_ellipsis(HEADER, usize::from(usable_width)),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            header_area,
        );
    }

    let body_height = area.height.saturating_sub(1);
    let status_height = u16::from(state.status.is_some() && body_height >= 2);
    let tree_height = body_height.saturating_sub(status_height);
    let tree_area = Rect::new(area.x, area.y.saturating_add(1), usable_width, tree_height);

    state.reconcile_scroll(usize::from(tree_height));
    if tree_area.width > 0 && tree_area.height > 0 {
        let lines = state
            .visible_rows()
            .into_iter()
            .skip(state.scroll)
            .take(usize::from(tree_height))
            .map(|row| {
                let style = if row.path == state.selected {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::styled(format_row(row, usize::from(usable_width)), style)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(Text::from(lines)), tree_area);
    }

    if status_height > 0 {
        let status_area = Rect::new(
            area.x,
            tree_area.y.saturating_add(tree_area.height),
            usable_width,
            1,
        );
        if let Some(status) = state.status() {
            frame.render_widget(
                Paragraph::new(clip_text_with_ellipsis(
                    status,
                    usize::from(status_area.width),
                ))
                .style(Style::default().fg(Color::Yellow)),
                status_area,
            );
        }
    }
}

fn format_row(row: VisibleRow<'_>, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let indent_width = row.depth.saturating_mul(INDENT_WIDTH).min(width);
    let glyph = if row.expanded {
        EXPANDED_GLYPH
    } else {
        COLLAPSED_GLYPH
    };
    let mut text = " ".repeat(indent_width);
    text.push(glyph);
    text.push(' ');
    text.push_str(&row.name.to_string_lossy());

    let mut fitted = clip_text_with_ellipsis(&text, width);
    let fitted_width = UnicodeWidthStr::width(fitted.as_str());
    fitted.push_str(&" ".repeat(width.saturating_sub(fitted_width)));
    fitted
}

fn display_name(path: &Path) -> OsString {
    path.file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| path.as_os_str().to_os_string())
}

fn sort_children(children: &mut [LoadedChild]) {
    children.sort_by(|left, right| {
        let left_key = left.name.to_string_lossy().to_lowercase();
        let right_key = right.name.to_string_lossy().to_lowercase();
        left_key
            .cmp(&right_key)
            .then_with(|| left.name.cmp(&right.name))
    });
}

fn load_directory(path: &Path) -> io::Result<Vec<LoadedChild>> {
    let entries = fs::read_dir(path)?.map(|entry| entry.and_then(loaded_directory_from_entry));
    collect_directory_children(entries)
}

fn loaded_directory_from_entry(entry: DirEntry) -> io::Result<Option<LoadedChild>> {
    let path = entry.path();
    let file_type = entry.file_type()?;
    let is_directory = if file_type.is_dir() {
        true
    } else if file_type.is_symlink() {
        fs::metadata(&path)?.is_dir()
    } else {
        false
    };

    Ok(is_directory.then(|| LoadedChild {
        path,
        name: entry.file_name(),
    }))
}

fn collect_directory_children<I>(entries: I) -> io::Result<Vec<LoadedChild>>
where
    I: IntoIterator<Item = io::Result<Option<LoadedChild>>>,
{
    let mut children = Vec::new();
    for entry in entries {
        if let Some(child) = entry? {
            children.push(child);
        }
    }
    sort_children(&mut children);
    Ok(children)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs::File;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use tempfile::tempdir;

    use super::*;

    fn child(parent: &Path, name: &str) -> LoadedChild {
        LoadedChild::directory(parent.join(name))
    }

    fn names(state: &ExplorerState) -> Vec<String> {
        state
            .visible_rows()
            .into_iter()
            .map(|row| row.name.to_string_lossy().into_owned())
            .collect()
    }

    fn draw(state: &mut ExplorerState, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), state))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .filter_map(|x| buffer.cell((x, y)))
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn new_root_is_selected_collapsed_unloaded_and_does_no_io() {
        let root = PathBuf::from("missing-root-is-fine");
        let state = ExplorerState::new(root.clone());

        assert_eq!(state.root(), root);
        assert_eq!(state.selected_path(), root);
        assert_eq!(state.scroll(), 0);
        assert_eq!(state.status(), None);
        let node = state.nodes.get(&root).unwrap();
        assert!(!node.expanded);
        assert!(node.children.is_none());
    }

    #[test]
    fn root_without_file_name_displays_the_path_itself() {
        let root = if cfg!(windows) {
            PathBuf::from(r"D:\")
        } else {
            PathBuf::from("/")
        };
        let state = ExplorerState::new(root.clone());
        assert_eq!(state.visible_rows()[0].name, root.as_os_str());
        assert_eq!(state.visible_rows()[0].path, root);
    }

    #[test]
    fn expand_is_lazy_collapse_retains_cache_and_reexpand_does_not_reload() {
        let root = PathBuf::from("root");
        let nested = root.join("a").join("nested");
        let calls = Cell::new(0);
        let mut state = ExplorerState::new(root.clone());

        assert!(state.expand_selected_with(|path| {
            calls.set(calls.get() + 1);
            assert_eq!(path, root);
            Ok(vec![child(path, "a"), child(path, "b")])
        }));
        assert_eq!(calls.get(), 1);
        assert_eq!(names(&state), ["root", "a", "b"]);
        assert!(!state.nodes.contains_key(&nested));

        assert!(state.collapse_selected());
        assert_eq!(names(&state), ["root"]);
        assert!(state.nodes[&root].children.is_some());
        assert!(state.expand_selected_with(|_| {
            calls.set(calls.get() + 1);
            Err(io::Error::other("must not reload"))
        }));
        assert_eq!(calls.get(), 1);
        assert_eq!(names(&state), ["root", "a", "b"]);
    }

    #[test]
    fn empty_directory_is_loaded_and_can_toggle_without_reloading() {
        let root = PathBuf::from("empty");
        let calls = Cell::new(0);
        let mut state = ExplorerState::new(root.clone());

        assert!(state.expand_selected_with(|_| {
            calls.set(calls.get() + 1);
            Ok(Vec::new())
        }));
        assert_eq!(state.nodes[&root].children, Some(Vec::new()));
        assert!(state.nodes[&root].expanded);
        assert!(!state.select_right());

        let expanded = draw(&mut state, 16, 3);
        assert!(row_text(&expanded, 1, 15).starts_with("▼ empty"));
        assert!(state.toggle_selected());
        let collapsed = draw(&mut state, 16, 3);
        assert!(row_text(&collapsed, 1, 15).starts_with("▶ empty"));
        assert!(state.toggle_selected());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn nested_expansion_preserves_iterative_depth_first_order() {
        let root = PathBuf::from("root");
        let a = root.join("a");
        let mut state = ExplorerState::new(root.clone());
        state
            .expand_selected_with(|path| Ok(vec![child(path, "a"), child(path, "b")]))
            .then_some(())
            .unwrap();
        assert!(state.select_next());
        assert_eq!(state.selected_path(), a);
        assert!(
            state.expand_selected_with(|path| { Ok(vec![child(path, "a1"), child(path, "a2")]) })
        );

        assert_eq!(names(&state), ["root", "a", "a1", "a2", "b"]);
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|row| row.depth)
                .collect::<Vec<_>>(),
            [0, 1, 2, 2, 1]
        );
    }

    #[test]
    fn flatten_handles_a_very_deep_manually_expanded_tree_iteratively() {
        let root = PathBuf::from("root");
        let mut state = ExplorerState::new(root.clone());
        let mut parent = root;
        for index in 0..10_000 {
            let path = PathBuf::from(format!("node-{index}"));
            state.nodes.get_mut(&parent).unwrap().expanded = true;
            state.nodes.get_mut(&parent).unwrap().children = Some(vec![path.clone()]);
            state
                .nodes
                .insert(path.clone(), TreeNode::unloaded(display_name(&path)));
            parent = path;
        }

        let rows = state.visible_rows();
        assert_eq!(rows.len(), 10_001);
        assert_eq!(rows.last().unwrap().depth, 10_000);
    }

    #[test]
    fn sorting_is_case_insensitive_with_raw_name_tie_break_and_keeps_hidden_dirs() {
        let root = PathBuf::from("root");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected_with(|path| {
            Ok(vec![
                child(path, "b"),
                child(path, "a"),
                child(path, ".hidden"),
                child(path, "A"),
            ])
        }));

        assert_eq!(names(&state), ["root", ".hidden", "A", "a", "b"]);
    }

    #[test]
    fn initial_and_iterator_like_partial_failures_do_not_commit() {
        let root = PathBuf::from("root");
        let mut state = ExplorerState::new(root.clone());
        state.scroll = 7;
        let selected_before = state.selected.clone();
        let partial = child(&root, "partial");

        assert!(!state.expand_selected_with(|_| {
            collect_directory_children(vec![
                Ok(Some(partial.clone())),
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            ])
        }));

        assert_eq!(state.selected, selected_before);
        assert_eq!(state.scroll, 7);
        assert!(state.nodes[&root].children.is_none());
        assert!(!state.nodes[&root].expanded);
        assert!(!state.nodes.contains_key(&partial.path));
        assert!(state.status().unwrap().contains("denied"));
    }

    #[test]
    fn reload_failure_retains_cache_expansion_selection_and_scroll() {
        let root = PathBuf::from("root");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected_with(|path| Ok(vec![child(path, "old")])));
        state.scroll = 4;
        let before = state.clone();

        assert!(!state.reload_selected_with(|_| Err(io::Error::other("reload failed"))));
        let mut expected = before;
        expected.status = state.status.clone();
        assert_eq!(state, expected);
        assert!(state.status().unwrap().contains("reload failed"));
    }

    #[test]
    fn successful_reload_replaces_children_sorts_and_retains_selected_expansion() {
        let root = PathBuf::from("root");
        let old = root.join("old");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected_with(|path| Ok(vec![child(path, "old")])));
        assert!(
            state.reload_selected_with(|path| { Ok(vec![child(path, "z"), child(path, "new")]) })
        );

        assert_eq!(state.selected_path(), root);
        assert!(state.nodes[&root].expanded);
        assert!(!state.nodes.contains_key(&old));
        assert_eq!(names(&state), ["root", "new", "z"]);
        assert_eq!(state.status(), None);
    }

    #[test]
    fn selection_moves_in_visible_order_and_clamps_at_both_ends() {
        let root = PathBuf::from("root");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected_with(|path| Ok(vec![child(path, "a"), child(path, "b")])));

        assert!(!state.select_previous());
        assert_eq!(state.selected_path(), root);
        assert!(state.select_next());
        assert_eq!(state.selected_path(), root.join("a"));
        assert!(state.select_next());
        assert_eq!(state.selected_path(), root.join("b"));
        assert!(!state.select_next());
        assert_eq!(state.selected_path(), root.join("b"));
        assert!(state.select_previous());
        assert_eq!(state.selected_path(), root.join("a"));
    }

    #[test]
    fn collapse_hides_descendants_without_changing_selected_path() {
        let root = PathBuf::from("root");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected_with(|path| Ok(vec![child(path, "a")])));
        assert!(state.collapse_selected());

        assert_eq!(state.selected_path(), root);
        assert_eq!(names(&state), ["root"]);
        assert!(state.nodes.contains_key(&root.join("a")));
    }

    #[test]
    fn left_and_right_follow_tree_navigation_semantics() {
        let root = PathBuf::from("root");
        let a = root.join("a");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.select_right_with(|path| Ok(vec![child(path, "a")])));
        assert_eq!(state.selected_path(), root);
        assert!(state.select_right());
        assert_eq!(state.selected_path(), a);
        assert!(state.select_left()); // collapsed child selects its parent, which is visible
        assert_eq!(state.selected_path(), root);
        assert!(state.select_left()); // expanded root collapses
        assert!(!state.select_left()); // root cannot escape the Explorer root
    }

    #[test]
    fn selection_reconciles_to_nearest_visible_ancestor() {
        let root = PathBuf::from("root");
        let a = root.join("a");
        let nested = a.join("nested");
        let mut state = ExplorerState::new(root.clone());
        state.nodes.get_mut(&root).unwrap().expanded = true;
        state.nodes.get_mut(&root).unwrap().children = Some(vec![a.clone()]);
        state.nodes.insert(
            a.clone(),
            TreeNode {
                name: OsString::from("a"),
                expanded: false,
                children: Some(vec![nested.clone()]),
            },
        );
        state
            .nodes
            .insert(nested.clone(), TreeNode::unloaded(OsString::from("nested")));
        state.selected = nested;

        state.reconcile_after_tree_change();
        assert_eq!(state.selected_path(), a);
    }

    #[test]
    fn scroll_handles_zero_one_and_both_selection_directions() {
        let root = PathBuf::from("root");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected_with(|path| {
            Ok((0..8)
                .map(|index| child(path, &format!("d{index}")))
                .collect())
        }));

        state.reconcile_scroll(0);
        assert_eq!(state.scroll(), 0);
        for _ in 0..8 {
            state.select_next();
        }
        state.reconcile_scroll(1);
        assert_eq!(state.scroll(), 8);
        state.reconcile_scroll(3);
        assert_eq!(state.scroll(), 6);
        state.selected = root;
        state.reconcile_scroll(3);
        assert_eq!(state.scroll(), 0);
    }

    #[test]
    fn collapse_and_reload_clamp_scroll_after_the_tree_shrinks() {
        let root = PathBuf::from("root");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected_with(|path| {
            Ok((0..8)
                .map(|index| child(path, &format!("d{index}")))
                .collect())
        }));
        state.scroll = 8;

        assert!(state.reload_selected_with(|path| Ok(vec![child(path, "only")])));
        assert_eq!(state.scroll(), 1);
        state.reconcile_scroll(4);
        assert_eq!(state.scroll(), 0);
        state.scroll = 1;
        assert!(state.collapse_selected());
        assert_eq!(state.scroll(), 0);
    }

    #[test]
    fn parent_and_arbitrary_rebase_reset_to_a_fresh_root_transactionally() {
        let temp = tempdir().unwrap();
        let old_root = temp.path().join("old");
        let new_root = temp.path().join("new");
        fs::create_dir_all(&old_root).unwrap();
        fs::create_dir_all(&new_root).unwrap();
        let mut state = ExplorerState::new(old_root.clone());
        assert!(state.expand_selected_with(|path| Ok(vec![child(path, "cached")])));
        state.scroll = 3;

        assert!(state.rebase(new_root.clone()));
        assert_eq!(state.root(), new_root);
        assert_eq!(state.selected_path(), new_root);
        assert_eq!(state.nodes.len(), 1);
        assert!(state.nodes[&new_root].children.is_none());
        assert_eq!(state.scroll(), 0);
        assert_eq!(state.status(), None);

        assert!(state.rebase_to_parent());
        assert_eq!(state.root(), temp.path());
        assert_eq!(state.selected_path(), temp.path());
        assert!(state.nodes[state.root()].children.is_none());
    }

    #[test]
    fn arbitrary_rebase_failure_preserves_old_tree_except_status() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let file = temp.path().join("file.txt");
        File::create(&file).unwrap();
        let mut state = ExplorerState::new(root);
        assert!(state.expand_selected_with(|path| Ok(vec![child(path, "cached")])));
        let before = state.clone();

        assert!(!state.rebase(file));
        let mut expected = before;
        expected.status = state.status.clone();
        assert_eq!(state, expected);
        assert!(state.status().unwrap().contains("not a directory"));
    }

    #[test]
    fn production_loader_is_one_level_directory_only_and_preserves_path_identity() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let a = root.join("a");
        let nested = a.join("nested");
        let b = root.join("b");
        let spaces = root.join("dir with spaces");
        let japanese = root.join("日本語");
        let hidden = root.join(".hidden");
        for directory in [&nested, &b, &spaces, &japanese, &hidden] {
            fs::create_dir_all(directory).unwrap();
        }
        File::create(root.join("file.txt")).unwrap();

        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected());
        let children = state.nodes[&root].children.as_ref().unwrap();
        assert_eq!(children.len(), 5);
        for path in [&a, &b, &spaces, &japanese, &hidden] {
            assert!(children.contains(path));
            assert!(state.nodes.contains_key(path));
        }
        assert!(!state.nodes.contains_key(&root.join("file.txt")));
        assert!(!state.nodes.contains_key(&nested));

        state.selected = a.clone();
        assert_eq!(state.selected_path(), a);
        assert!(state.expand_selected());
        assert_eq!(state.nodes[&a].children, Some(vec![nested.clone()]));
        assert!(state.nodes.contains_key(&nested));

        state.selected = japanese.clone();
        assert_eq!(state.selected_path(), japanese);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_directory_identity_is_preserved_and_rendering_does_not_panic() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempdir().unwrap();
        let name = OsString::from_vec(vec![b'n', 0x80]);
        let directory = temp.path().join(&name);
        fs::create_dir(&directory).unwrap();
        let mut state = ExplorerState::new(temp.path().to_path_buf());

        assert!(state.expand_selected());
        assert!(state.nodes.contains_key(&directory));
        state.selected = directory.clone();
        assert_eq!(state.selected_path(), directory);
        let _ = draw(&mut state, 20, 4);
    }

    #[test]
    fn directory_symlink_is_visible_and_expandable_when_supported() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let target = root.join("target");
        let nested = target.join("nested");
        let link = root.join("link");
        fs::create_dir_all(&nested).unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(&target, &link) {
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
            ) {
                return;
            }
            panic!("could not create directory symlink: {error}");
        }

        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected());
        assert!(state.nodes.contains_key(&link));
        state.selected = link.clone();
        assert!(state.expand_selected());
        assert_eq!(state.nodes[&link].children, Some(vec![link.join("nested")]));
    }

    #[test]
    fn renderer_shows_header_glyphs_and_nested_indentation() {
        let root = PathBuf::from("root");
        let a = root.join("a");
        let mut state = ExplorerState::new(root.clone());
        assert!(state.expand_selected_with(|path| Ok(vec![child(path, "a")])));
        state.selected = a.clone();
        assert!(state.expand_selected_with(|path| Ok(vec![child(path, "nested")])));
        state.selected = root;

        let buffer = draw(&mut state, 24, 6);
        assert!(row_text(&buffer, 0, 23).starts_with(HEADER));
        assert!(
            buffer
                .cell((0, 0))
                .unwrap()
                .modifier
                .contains(Modifier::BOLD)
        );
        assert!(row_text(&buffer, 1, 23).starts_with("▼ root"));
        assert!(row_text(&buffer, 2, 23).starts_with("  ▼ a"));
        assert!(row_text(&buffer, 3, 23).starts_with("    ▶ nested"));
    }

    #[test]
    fn selected_row_highlights_full_usable_width_but_not_right_border() {
        let mut state = ExplorerState::new(PathBuf::from("root"));
        let buffer = draw(&mut state, 12, 3);

        assert!(row_text(&buffer, 1, 11).starts_with("▶ root"));
        for x in 0..11 {
            let modifier = buffer.cell((x, 1)).unwrap().modifier;
            assert!(modifier.contains(Modifier::BOLD));
            assert!(modifier.contains(Modifier::REVERSED));
        }
        assert_eq!(buffer.cell((11, 1)).unwrap().symbol(), "│");
        assert!(
            !buffer
                .cell((11, 1))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn renderer_clips_long_ascii_and_japanese_names_to_display_width() {
        let root = PathBuf::from("日本語ディレクトリ名");
        let mut state = ExplorerState::new(root);
        let buffer = draw(&mut state, 10, 3);
        let rendered = row_text(&buffer, 1, 9);
        assert!(rendered.contains('…'));
        assert!(!rendered.contains('�'));

        let mut ascii = ExplorerState::new(PathBuf::from("a-very-long-directory-name"));
        let buffer = draw(&mut ascii, 10, 3);
        assert!(row_text(&buffer, 1, 9).contains('…'));
    }

    #[test]
    fn renderer_survives_zero_and_tiny_geometry() {
        for (width, height) in [(0, 0), (0, 4), (4, 0), (1, 1), (1, 4), (2, 1), (2, 2)] {
            let mut state = ExplorerState::new(PathBuf::from("root"));
            let _ = draw(&mut state, width, height);
            if width == 0 || height <= 1 {
                assert_eq!(state.scroll(), 0);
            }
        }
    }

    #[test]
    fn renderer_shows_clipped_status_only_when_tree_keeps_a_row() {
        let mut state = ExplorerState::new(PathBuf::from("root"));
        state.status = Some("Permission denied while reading a very long path".to_string());

        let buffer = draw(&mut state, 18, 4);
        assert!(row_text(&buffer, 3, 17).contains("Permission"));
        assert!(row_text(&buffer, 3, 17).contains('…'));
        assert!(row_text(&buffer, 1, 17).contains("root"));

        let tiny = draw(&mut state, 18, 2);
        assert!(row_text(&tiny, 1, 17).contains("root"));
        assert!(!row_text(&tiny, 1, 17).contains("Permission"));
    }
}
