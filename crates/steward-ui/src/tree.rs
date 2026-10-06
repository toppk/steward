use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::base::ScrollbarAxis;
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Div, Entity, FocusHandle, Focusable as _, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Rems, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    UniformListScrollHandle, Window, div, relative, rems, uniform_list,
};
use gpui_omarchy::{
    ActiveTheme as _, ButtonVariant, Theme, button, input, scrollbar, with_tooltip,
};
use steward_proto::{Entry, Kind, LocateCheck, LocateMode, Request, wire};

use crate::client::{fetch, print_info, print_warning, scanning};
use crate::format;

const SCANNING: &str = "stewardd is scanning: totals here may be stale (or 0 on a root's first \
                        scan) until it finishes; this view reloads then.";

/// What the tree asks of the rest of the window.
pub enum TreeEvent {
    /// Show every location of this content id.
    ShowContent(String),
}

impl gpui_kit::EventEmitter<TreeEvent> for TreeView {}

const ROW_H: Rems = Rems(1.5);
const INDENT: f32 = 1.1;
const COL_BAR: Rems = Rems(7.0);
const COL_PCT: Rems = Rems(4.0);
const COL_SIZE: Rems = Rems(6.5);
const COL_COUNT: Rems = Rems(9.0);
const COL_DATE: Rems = Rems(6.5);
const COL_TAGS: Rems = Rems(12.0);
/// Locate results shown; the list only draws what is on screen.
const LOCATE_LIMIT: u32 = 100_000;

#[derive(Clone, Debug)]
struct Row {
    entry: Entry,
    depth: usize,
    expanded: bool,
    loading: bool,
    /// The parent's subtree totals, for the percentage column.
    parent_alloc: u64,
    parent_items: u64,
}

impl Row {
    fn is_dir(&self) -> bool {
        self.entry.kind == Kind::Dir
    }

    fn name(&self) -> &str {
        if self.depth == 0 {
            return &self.entry.path;
        }
        self.entry
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&self.entry.path)
    }
}

/// What the tree ranks and measures by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    /// Space on disk.
    Space,
    /// Entries beneath: files, directories, symlinks… (roughly inodes).
    Items,
}

impl Measure {
    fn of(self, e: &Entry) -> u64 {
        match self {
            Self::Space => e.total_alloc,
            Self::Items => e.total_items,
        }
    }

    fn parent(self, r: &Row) -> u64 {
        match self {
            Self::Space => r.parent_alloc,
            Self::Items => r.parent_items,
        }
    }

    fn show(self, e: &Entry) -> String {
        match self {
            Self::Space => format::bytes(e.total_alloc),
            Self::Items => format::count(e.total_items),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Tree,
    Locate,
}

pub struct TreeView {
    pub focus: FocusHandle,
    root: PathBuf,
    rows: Vec<Row>,
    children: HashMap<String, Vec<Entry>>,
    selected: usize,
    scroll: UniformListScrollHandle,
    mode: Mode,
    locate: Entity<InputState>,
    hits: Vec<String>,
    hit_selected: usize,
    hit_scroll: UniformListScrollHandle,
    message: Option<SharedString>,
    /// The daemon is mid-scan, so directory totals may still be zero.
    scanning: bool,
    measure: Measure,
    locate_mode: LocateMode,
    locate_ignore_case: bool,
    polling: bool,
    _subscriptions: Vec<Subscription>,
}

impl TreeView {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let locate = cx.new(|cx| {
            InputState::new(window, cx).placeholder("locate: substring, or glob like *.CR3")
        });
        let sub = cx.subscribe_in(&locate, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.run_locate(cx);
                window.focus(&this.focus, cx);
            }
        });
        let mut this = Self {
            focus: cx.focus_handle(),
            root,
            rows: Vec::new(),
            children: HashMap::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            mode: Mode::Tree,
            locate,
            hits: Vec::new(),
            hit_selected: 0,
            hit_scroll: UniformListScrollHandle::new(),
            message: None,
            scanning: false,
            polling: false,
            measure: Measure::Space,
            locate_mode: LocateMode::Auto,
            locate_ignore_case: false,
            _subscriptions: vec![sub],
        };
        this.load_root(None, cx);
        this
    }

    /// Show `root` as the top row, expanded; then select `reveal` if given.
    fn load_root(&mut self, reveal: Option<String>, cx: &mut Context<'_, Self>) {
        self.rows.clear();
        self.children.clear();
        self.selected = 0;
        self.mode = Mode::Tree;
        self.message = Some("loading…".into());
        let path = self.root.clone();
        let task = cx
            .background_executor()
            .spawn(async move { fetch(move |c| Ok((c.stat(path)?, scanning(c)?)))() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((entry, scanning)) => {
                        if scanning && !this.scanning {
                            print_warning(SCANNING);
                        }
                        this.scanning = scanning;
                        if scanning {
                            this.poll_scan(cx);
                        }
                        let (alloc, items) = (entry.total_alloc, entry.total_items);
                        this.rows = vec![Row {
                            entry,
                            depth: 0,
                            expanded: false,
                            loading: false,
                            parent_alloc: alloc,
                            parent_items: items,
                        }];
                        this.message = None;
                        this.expand(0, reveal, cx);
                    }
                    Err(e) => this.warn(format!(
                        "{e}. Is stewardd running, and is this path under a [[root]] in \
                         settings.toml?"
                    )),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Reload once the daemon's scan ends, so totals and order are real.
    fn poll_scan(&mut self, cx: &mut Context<'_, Self>) {
        if self.polling {
            return;
        }
        self.polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(5))
                    .await;
                let busy = cx
                    .background_executor()
                    .spawn(async move { fetch(scanning)() })
                    .await
                    .unwrap_or(false);
                if !busy {
                    let _ = this.update(cx, |this, cx| {
                        this.polling = false;
                        this.scanning = false;
                        let keep = this.rows.get(this.selected).map(|r| r.entry.path.clone());
                        let root = this.root.clone();
                        this.enter(root, keep, cx);
                    });
                    return;
                }
            }
        })
        .detach();
    }

    fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        print_warning(&msg);
        self.message = Some(msg.into());
    }

    fn note(&mut self, msg: String) {
        print_info(&msg);
        self.message = Some(msg.into());
    }

    fn expand(&mut self, ix: usize, reveal: Option<String>, cx: &mut Context<'_, Self>) {
        let Some(row) = self.rows.get(ix) else { return };
        if !row.is_dir() || row.expanded || row.loading {
            return;
        }
        let path = row.entry.path.clone();
        if let Some(kids) = self.children.get(&path).cloned() {
            self.insert_children(ix, kids);
            self.reveal(reveal);
            cx.notify();
            return;
        }
        self.rows[ix].loading = true;
        let p = wire::to_path(&path);
        let task = cx
            .background_executor()
            .spawn(async move { fetch(move |c| c.children(p))() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                // Rows may have moved while the request was out.
                let Some(ix) = this.rows.iter().position(|r| r.entry.path == path) else {
                    return;
                };
                this.rows[ix].loading = false;
                match result {
                    Ok(kids) => {
                        this.children.insert(path, kids.clone());
                        this.insert_children(ix, kids);
                        this.reveal(reveal);
                    }
                    Err(e) => this.warn(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn insert_children(&mut self, ix: usize, mut kids: Vec<Entry>) {
        let depth = self.rows[ix].depth + 1;
        let parent_alloc = self.rows[ix].entry.total_alloc;
        let parent_items = self.rows[ix].entry.total_items;
        self.rows[ix].expanded = true;
        let measure = self.measure;
        kids.sort_by_key(|e| std::cmp::Reverse(measure.of(e)));
        let new = kids.into_iter().map(|entry| Row {
            entry,
            depth,
            expanded: false,
            loading: false,
            parent_alloc,
            parent_items,
        });
        self.rows.splice(ix + 1..ix + 1, new);
    }

    /// Rank by `measure` instead, keeping what is expanded and selected.
    fn set_measure(&mut self, measure: Measure, cx: &mut Context<'_, Self>) {
        if measure == self.measure || self.rows.is_empty() {
            self.measure = measure;
            cx.notify();
            return;
        }
        self.measure = measure;
        let expanded: std::collections::HashSet<String> = self
            .rows
            .iter()
            .filter(|r| r.expanded)
            .map(|r| r.entry.path.clone())
            .collect();
        let selected = self.rows.get(self.selected).map(|r| r.entry.path.clone());
        let mut root = self.rows[0].clone();
        root.expanded = false;
        self.rows = vec![root];
        // Re-expand in order, from the cache: children are already known.
        let mut ix = 0;
        while ix < self.rows.len() {
            let path = self.rows[ix].entry.path.clone();
            if expanded.contains(&path)
                && let Some(kids) = self.children.get(&path).cloned()
            {
                self.insert_children(ix, kids);
            }
            ix += 1;
        }
        self.selected = selected
            .and_then(|p| self.rows.iter().position(|r| r.entry.path == p))
            .unwrap_or(0);
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    fn collapse(&mut self, ix: usize) {
        let Some(row) = self.rows.get_mut(ix) else {
            return;
        };
        if !row.expanded {
            return;
        }
        row.expanded = false;
        let depth = row.depth;
        let end = self.rows[ix + 1..]
            .iter()
            .position(|r| r.depth <= depth)
            .map_or(self.rows.len(), |n| ix + 1 + n);
        self.rows.drain(ix + 1..end);
        if self.selected > ix && self.selected < end {
            self.selected = ix;
        } else if self.selected >= end {
            self.selected -= end - ix - 1;
        }
    }

    fn reveal(&mut self, path: Option<String>) {
        if let Some(i) = path.and_then(|p| self.rows.iter().position(|r| r.entry.path == p)) {
            self.select(i);
        }
    }

    fn parent_of(&self, ix: usize) -> Option<usize> {
        let depth = self.rows.get(ix)?.depth;
        self.rows[..ix].iter().rposition(|r| r.depth < depth)
    }

    fn select(&mut self, ix: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = ix.min(self.rows.len() - 1);
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
    }

    fn toggle(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if self.rows.get(ix).is_some_and(|r| r.expanded) {
            self.collapse(ix);
        } else {
            self.expand(ix, None, cx);
        }
        cx.notify();
    }

    /// Make a directory the new top of the tree.
    fn enter(&mut self, path: PathBuf, reveal: Option<String>, cx: &mut Context<'_, Self>) {
        self.root = path;
        self.load_root(reveal, cx);
    }

    fn rescan_selected(&mut self, cx: &mut Context<'_, Self>) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let path = if row.is_dir() {
            wire::to_path(&row.entry.path)
        } else {
            wire::to_path(&row.entry.path)
                .parent()
                .map_or_else(PathBuf::new, Path::to_path_buf)
        };
        let target = wire::display(&wire::path(&path));
        self.message = Some(format!("rescanning {target}…").into());
        let task = cx.background_executor().spawn(async move {
            fetch(move |c| {
                c.request(&Request::Scan {
                    path,
                    trust_dir_mtime: false,
                })
            })()
        });
        let keep = self.rows.get(self.selected).map(|r| r.entry.path.clone());
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(v) => {
                        this.note(format!(
                            "rescanned {target}: +{} ~{} -{} in {} ms",
                            v["inserted"], v["updated"], v["deleted"], v["millis"]
                        ));
                        let root = this.root.clone();
                        let msg = this.message.clone();
                        this.enter(root, keep, cx);
                        this.message = msg;
                    }
                    Err(e) => this.warn(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn run_locate(&mut self, cx: &mut Context<'_, Self>) {
        let pattern = self.locate.read(cx).value().to_string();
        if pattern.trim().is_empty() {
            self.mode = Mode::Tree;
            cx.notify();
            return;
        }
        self.message = Some(format!("locating {pattern}…").into());
        let (mode, ignore_case) = (self.locate_mode, self.locate_ignore_case);
        let task = cx.background_executor().spawn({
            let pattern = pattern.clone();
            async move {
                // Results are confirmed on disk; folders of gone ones rescanned.
                fetch(move |c| {
                    c.locate_checked(
                        pattern,
                        LOCATE_LIMIT,
                        mode,
                        ignore_case,
                        None,
                        LocateCheck::Rescan,
                    )
                })()
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(found) => {
                        let hits = found.paths;
                        let mut note =
                            format!("{} matches for {pattern}", format::count(hits.len() as u64));
                        if hits.len() as u32 == LOCATE_LIMIT {
                            note.push_str(" (stopped at the limit: narrow the pattern)");
                        }
                        if !found.rescanned.is_empty() {
                            note.push_str(&format!(
                                " · rescanned {} folder(s) where results had gone",
                                found.rescanned.len()
                            ));
                        }
                        if !found.stale.is_empty() {
                            note.push_str(&format!(
                                " · {} gone from disk, left out",
                                found.stale.len()
                            ));
                        }
                        this.note(note);
                        this.hits = hits;
                        this.hit_selected = 0;
                        this.mode = Mode::Locate;
                    }
                    Err(e) => this.warn(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Show `path` as the top of the tree.
    pub fn set_root(&mut self, path: PathBuf, cx: &mut Context<'_, Self>) {
        self.enter(path, None, cx);
    }

    /// Open `path`'s directory and select it.
    pub fn reveal_path(&mut self, path: &str, cx: &mut Context<'_, Self>) {
        let parent = wire::to_path(path)
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        self.enter(parent, Some(path.to_string()), cx);
    }

    fn open_hit(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        let Some(hit) = self.hits.get(ix).cloned() else {
            return;
        };
        let parent = wire::to_path(&hit)
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        self.enter(parent, Some(hit), cx);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let ks = &ev.keystroke;
        let key = ks.key.as_str();
        let searching = self.locate.read(cx).focus_handle(cx).is_focused(window);
        if searching {
            if key == "escape" || key == "down" {
                window.focus(&self.focus, cx);
                cx.notify();
            }
            return;
        }
        if key == "/" || (ks.modifiers.control && key == "f") {
            self.locate.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
            return;
        }
        match self.mode {
            Mode::Tree => self.on_tree_key(key, cx),
            Mode::Locate => self.on_locate_key(key, cx),
        }
    }

    fn on_tree_key(&mut self, key: &str, cx: &mut Context<'_, Self>) {
        let page = 20;
        let sel = self.selected;
        match key {
            "down" | "j" => self.select(sel + 1),
            "up" | "k" => self.select(sel.saturating_sub(1)),
            "pagedown" => self.select(sel + page),
            "pageup" => self.select(sel.saturating_sub(page)),
            "home" => self.select(0),
            "end" => self.select(usize::MAX),
            "right" | "l" => {
                if self.rows.get(sel).is_some_and(|r| r.expanded) {
                    self.select(sel + 1);
                } else {
                    self.expand(sel, None, cx);
                }
            }
            "left" | "h" => {
                if self.rows.get(sel).is_some_and(|r| r.expanded) {
                    self.collapse(sel);
                } else if let Some(p) = self.parent_of(sel) {
                    self.select(p);
                }
            }
            "enter" | "space" => self.toggle(sel, cx),
            "g" => {
                if let Some(r) = self.rows.get(sel).filter(|r| r.is_dir()) {
                    self.enter(wire::to_path(&r.entry.path), None, cx);
                }
            }
            "backspace" | "u" => {
                if let Some(up) = self.root.parent().map(Path::to_path_buf) {
                    let here = wire::path(&self.root);
                    self.enter(up, Some(here), cx);
                }
            }
            "r" | "f5" => self.rescan_selected(cx),
            "i" => {
                let next = match self.measure {
                    Measure::Space => Measure::Items,
                    Measure::Items => Measure::Space,
                };
                self.set_measure(next, cx);
            }
            _ => return,
        }
        cx.notify();
    }

    fn on_locate_key(&mut self, key: &str, cx: &mut Context<'_, Self>) {
        let n = self.hits.len();
        match key {
            "down" | "j" if n > 0 => self.hit_selected = (self.hit_selected + 1).min(n - 1),
            "up" | "k" => self.hit_selected = self.hit_selected.saturating_sub(1),
            "enter" => {
                self.open_hit(self.hit_selected, cx);
                return;
            }
            "escape" => self.mode = Mode::Tree,
            _ => return,
        }
        self.hit_scroll
            .scroll_to_item(self.hit_selected, ScrollStrategy::Nearest);
        cx.notify();
    }
}

fn cell(w: Rems) -> Div {
    div().w(w).flex_shrink_0().px(rems(0.5)).overflow_hidden()
}

fn right(w: Rems) -> Div {
    cell(w).flex().justify_end()
}

fn tag_color(tag: &str, theme: &Theme) -> Hsla {
    match tag.rsplit(':').next().unwrap_or(tag) {
        "ignored" | "build-output" | "cache" | "venv" | "dependencies" => theme.warning,
        "trash" => theme.danger,
        "repo" | "vcs-metadata" => theme.success,
        _ => theme.secondary,
    }
}

fn tree_row(
    row: &Row,
    ix: usize,
    selected: bool,
    measure: Measure,
    theme: &Theme,
    cx: &mut Context<'_, TreeView>,
) -> impl IntoElement {
    let e = &row.entry;
    let whole = measure.parent(row);
    let pct = if whole == 0 {
        0.0
    } else {
        measure.of(e) as f32 / whole as f32
    };
    let disclosure = match (row.is_dir(), row.expanded, row.loading) {
        (_, _, true) => "…",
        (true, true, _) => "▾",
        (true, false, _) => "▸",
        _ => " ",
    };
    let label = e
        .tags
        .iter()
        .map(|t| t.rsplit(':').next().unwrap_or(t))
        .collect::<Vec<_>>();
    let tag_col = label
        .first()
        .map_or(theme.secondary, |t| tag_color(t, theme));
    let mut tags = label.join(" ");
    for extra in [e.category.as_deref(), e.content_id.as_ref().map(|_| "cid")]
        .into_iter()
        .flatten()
    {
        if !tags.is_empty() {
            tags.push(' ');
        }
        tags.push_str(extra);
    }
    let name_color = if row.is_dir() {
        theme.bright
    } else {
        theme.foreground
    };
    div()
        .id(ix)
        .w_full()
        .h(ROW_H)
        .flex()
        .flex_row()
        .items_center()
        .when(selected, |d| d.bg(theme.selection))
        .hover(|d| d.bg(theme.surface))
        .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
            this.selected = ix;
            if ev.click_count() >= 2 {
                this.toggle(ix, cx);
            }
            window.focus(&this.focus, cx);
            cx.notify();
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .pl(rems(0.5 + INDENT * row.depth as f32))
                .child(
                    div()
                        .id(("disclose", ix))
                        .w(rems(1.0))
                        .flex_shrink_0()
                        .text_color(theme.secondary)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.selected = ix;
                            this.toggle(ix, cx);
                        }))
                        .child(disclosure),
                )
                .child(with_tooltip(
                    div()
                        .id(("name", ix))
                        .min_w_0()
                        .truncate()
                        .text_color(name_color)
                        .child(wire::display(row.name())),
                    wire::display(row.name()),
                )),
        )
        .child(
            cell(COL_BAR).child(
                div().h(rems(0.625)).w_full().bg(theme.inset).child(
                    div()
                        .h_full()
                        .w(relative(pct.clamp(0.0, 1.0)))
                        .bg(theme.accent),
                ),
            ),
        )
        .child(
            right(COL_PCT)
                .text_color(theme.secondary)
                .child(format!("{:.1}%", pct * 100.0)),
        )
        .child(right(COL_SIZE).child(measure.show(e)))
        .child(
            right(COL_COUNT)
                .text_color(theme.secondary)
                .child(if row.is_dir() {
                    format!(
                        "{}/{}",
                        format::count(e.total_dirs.saturating_sub(1)),
                        format::count(e.total_files)
                    )
                } else {
                    String::new()
                }),
        )
        .child(
            right(COL_DATE)
                .text_color(theme.secondary)
                .child(format::date(e.mtime)),
        )
        .child(cell(COL_TAGS).text_color(tag_col).child(tags))
}

fn header(measure: Measure, theme: &Theme) -> Div {
    div()
        .h(ROW_H)
        .flex()
        .flex_row()
        .items_center()
        .border_b_1()
        .border_color(theme.divider())
        .text_color(theme.secondary)
        .child(div().flex_1().pl(rems(1.5)).child("Name"))
        .child(cell(COL_BAR).child("Subtree"))
        .child(right(COL_PCT).child("%"))
        .child(right(COL_SIZE).child(match measure {
            Measure::Space => "Size",
            Measure::Items => "Items",
        }))
        .child(right(COL_COUNT).child("Dirs/Files"))
        .child(right(COL_DATE).child("Modified"))
        .child(cell(COL_TAGS).child("Class"))
}

fn status_line(row: Option<&Row>, theme: &Theme, cx: &mut Context<'_, TreeView>) -> Div {
    let bar = div()
        .flex()
        .flex_row()
        .gap(rems(1.25))
        .px(rems(0.75))
        .py(rems(0.375))
        .border_t_1()
        .border_color(theme.divider())
        .bg(theme.surface)
        .text_color(theme.secondary);
    let Some(r) = row else { return bar };
    let e = &r.entry;
    let k = match e.kind {
        Kind::Dir => 'd',
        Kind::Symlink => 'l',
        Kind::File => '-',
        Kind::Other => '?',
    };
    bar.child(
        div()
            .text_color(theme.bright)
            .overflow_hidden()
            .child(wire::display(&e.path)),
    )
    .child(format::mode(k, e.mode))
    .child(format!("{}:{}", format::user(e.uid), e.gid))
    .child(format!("{} on disk", format::bytes(e.total_alloc)))
    .child(format!("{} apparent", format::bytes(e.total_size)))
    .when(r.is_dir(), |d| {
        d.child(format!(
            "{} dirs, {} files, {} items",
            format::count(e.total_dirs.saturating_sub(1)),
            format::count(e.total_files),
            format::count(e.total_items)
        ))
    })
    .when_some(e.content_id.clone(), |d, id| {
        let short = format!("{}…", &id[..id.len().min(21)]);
        d.child(with_tooltip(
            div().id("cid").text_color(theme.accent).child(short),
            id.clone(),
        ))
        .child(
            button("locations", "All locations", ButtonVariant::Secondary, cx).on_click(
                cx.listener(move |_, _, _, cx| cx.emit(TreeEvent::ShowContent(id.clone()))),
            ),
        )
    })
}

impl Render for TreeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.omarchy().clone();

        let list = match self.mode {
            Mode::Tree => uniform_list(
                "tree",
                self.rows.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                    let theme = cx.omarchy().clone();
                    range
                        .map(|ix| {
                            let row = this.rows[ix].clone();
                            tree_row(&row, ix, ix == this.selected, this.measure, &theme, cx)
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .size_full(),
            Mode::Locate => uniform_list(
                "hits",
                self.hits.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                    let theme = cx.omarchy().clone();
                    range
                        .map(|ix| {
                            div()
                                .id(ix)
                                .w_full()
                                .h(ROW_H)
                                .px(rems(0.75))
                                .flex()
                                .items_center()
                                .when(ix == this.hit_selected, |d| d.bg(theme.selection))
                                .hover(|d| d.bg(theme.surface))
                                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                                    this.hit_selected = ix;
                                    if ev.click_count() >= 2 {
                                        this.open_hit(ix, cx);
                                    }
                                    cx.notify();
                                }))
                                .child(with_tooltip(
                                    div()
                                        .id(("hit", ix))
                                        .min_w_0()
                                        .truncate()
                                        .child(wire::display(&this.hits[ix])),
                                    wire::display(&this.hits[ix]),
                                ))
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.hit_scroll)
            .size_full(),
        };
        // The list fills what the toolbar and header leave, with a scrollbar
        // over its right edge on the same scroll handle.
        let bar = match self.mode {
            Mode::Tree => scrollbar(
                "tree-scrollbar",
                ScrollbarAxis::Vertical,
                &self.scroll,
                window,
                cx,
            ),
            Mode::Locate => scrollbar(
                "hits-scrollbar",
                ScrollbarAxis::Vertical,
                &self.hit_scroll,
                window,
                cx,
            ),
        };
        let list = div().relative().flex_1().min_h_0().child(list).child(bar);

        let up_root = self.root.clone();
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(rems(0.5))
            .px(rems(0.75))
            .py(rems(0.5))
            .border_b_1()
            .border_color(theme.divider())
            .child(
                button("up", "Up", ButtonVariant::Secondary, cx).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(p) = up_root.parent() {
                            this.enter(p.to_path_buf(), Some(wire::path(&up_root)), cx);
                        }
                    },
                )),
            )
            .child(
                button("rescan", "Rescan", ButtonVariant::Secondary, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.rescan_selected(cx))),
            )
            .child(
                div()
                    .pl(rems(0.5))
                    .text_color(theme.secondary)
                    .child("Rank by"),
            )
            .child(with_tooltip(
                button(
                    "by-space",
                    "Space",
                    if self.measure == Measure::Space {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_measure(Measure::Space, cx))),
                "rank by space on disk (i toggles)",
            ))
            .child(with_tooltip(
                button(
                    "by-items",
                    "Items",
                    if self.measure == Measure::Items {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_measure(Measure::Items, cx))),
                "rank by entries beneath — files, directories, symlinks — \
                 roughly the inodes and filesystem metadata they use (i toggles)",
            ))
            .child(
                div()
                    .flex_1()
                    .child(input("locate", &self.locate, window, cx)),
            )
            .children(
                [
                    (
                        LocateMode::Auto,
                        "Contains",
                        "substring, or a glob if the pattern has * ? [",
                    ),
                    (LocateMode::Exact, "Exact", "the whole name, literally"),
                    (
                        LocateMode::Glob,
                        "Glob",
                        "the whole name as a glob: * ? [abc]",
                    ),
                    (
                        LocateMode::Regex,
                        "Regex",
                        "a regular expression anywhere in the name",
                    ),
                ]
                .into_iter()
                .map(|(mode, label, tip)| {
                    with_tooltip(
                        button(
                            ("locate-mode", mode as usize),
                            label,
                            if self.locate_mode == mode {
                                ButtonVariant::Primary
                            } else {
                                ButtonVariant::Secondary
                            },
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.locate_mode = mode;
                            if this.mode == Mode::Locate {
                                this.run_locate(cx);
                            }
                            cx.notify();
                        })),
                        tip,
                    )
                }),
            )
            .child(with_tooltip(
                button(
                    "locate-case",
                    "Aa",
                    if self.locate_ignore_case {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.locate_ignore_case = !this.locate_ignore_case;
                    if this.mode == Mode::Locate {
                        this.run_locate(cx);
                    }
                    cx.notify();
                })),
                "ignore case (substrings always do)",
            ))
            .when(self.mode == Mode::Locate, |d| {
                d.child(
                    button("back", "Back to tree", ButtonVariant::Secondary, cx).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.mode = Mode::Tree;
                            cx.notify();
                        }),
                    ),
                )
            });

        let status = match self.mode {
            Mode::Tree => status_line(self.rows.get(self.selected).cloned().as_ref(), &theme, cx),
            Mode::Locate => status_line(None, &theme, cx),
        };

        div()
            .track_focus(&self.focus)
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, window, cx| this.on_key(ev, window, cx)),
            )
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font.clone())
            .text_size(rems(0.8125))
            .child(toolbar)
            .when(self.mode == Mode::Tree, |d| {
                d.child(header(self.measure, &theme))
            })
            .child(list)
            .when(self.scanning, |d| {
                d.child(
                    div()
                        .px(rems(0.75))
                        .py(rems(0.25))
                        .text_color(theme.warning)
                        .child(SCANNING),
                )
            })
            .children(self.message.clone().map(|m| {
                div()
                    .px(rems(0.75))
                    .py(rems(0.25))
                    .text_color(theme.warning)
                    .child(m)
            }))
            .child(status)
            .child(
                div()
                    .px(rems(0.75))
                    .pb(rems(0.25))
                    .bg(theme.surface)
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .child(
                        "↑↓ move · → expand · ← collapse/parent · enter toggle · g go into · \
                         u up · r rescan · / locate · esc back",
                    ),
            )
    }
}
