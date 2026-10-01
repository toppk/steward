//! Editing roots through the daemon's settings API (`settings`, `put_root`,
//! `remove_root`); the daemon validates, writes settings.toml and applies it.

use std::path::{Path, PathBuf};

use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Div, Entity, EventEmitter, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, rems,
};
use gpui_omarchy::{
    ActiveTheme as _, ButtonVariant, Status, Theme, alert, badge, button, input, number_input,
    switch,
};
use steward_proto::{Request, RootSettings};

use crate::client::{RootInfo, fetch, print_info, print_warning};
use crate::format;

pub enum SettingsEvent {
    /// Roots were added, changed or removed.
    Changed,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    Existing(PathBuf),
    New,
}

pub struct SettingsView {
    pub focus: FocusHandle,
    roots: Vec<RootInfo>,
    file: String,
    selected: Option<Selection>,
    /// The form's state; lists live here, scalars in the inputs below.
    draft: RootSettings,
    path_in: Entity<InputState>,
    interval_in: Entity<InputState>,
    full_in: Entity<InputState>,
    exclude_in: Entity<InputState>,
    contentid_in: Entity<InputState>,
    /// Inputs need a window to be refilled, so that happens at next render.
    refill: bool,
    status: Option<(Status, String)>,
    confirm_remove: bool,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

fn number(window: &mut Window, cx: &mut Context<'_, SettingsView>, min: f64) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .step(1.0)
            .min(min)
            .max(100_000.0)
    })
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let path_in = cx.new(|cx| InputState::new(window, cx).placeholder("/absolute/path or ~/…"));
        let exclude_in = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("gitignore pattern: /down/big, *.iso, node_modules/")
        });
        let contentid_in = cx.new(|cx| {
            InputState::new(window, cx).placeholder("folder under this root, e.g. Movies")
        });
        let subs = vec![
            cx.subscribe_in(
                &exclude_in,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.add_exclude(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &contentid_in,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.add_contentid(window, cx);
                    }
                },
            ),
        ];
        Self {
            focus: cx.focus_handle(),
            roots: Vec::new(),
            file: String::new(),
            selected: None,
            draft: RootSettings::new(PathBuf::new()),
            path_in,
            interval_in: number(window, cx, 1.0),
            full_in: number(window, cx, 0.0),
            exclude_in,
            contentid_in,
            refill: true,
            status: None,
            confirm_remove: false,
            busy: false,
            _subscriptions: subs,
        }
    }

    pub fn set_roots(
        &mut self,
        roots: Vec<RootInfo>,
        current: Option<PathBuf>,
        file: String,
        cx: &mut Context<'_, Self>,
    ) {
        self.roots = roots;
        self.file = file;
        let keep = match &self.selected {
            Some(Selection::Existing(p)) => self.roots.iter().any(|r| &r.settings.path == p),
            Some(Selection::New) => true,
            None => false,
        };
        match (&self.selected, keep) {
            (Some(Selection::Existing(p)), true) => {
                let p = p.clone();
                self.select_path(&p, cx);
            }
            (Some(Selection::New), true) => {}
            _ => match current.or_else(|| self.roots.first().map(|r| r.settings.path.clone())) {
                Some(p) => self.select_path(&p, cx),
                None => self.select_new(cx),
            },
        }
        cx.notify();
    }

    pub fn select_path(&mut self, path: &Path, cx: &mut Context<'_, Self>) {
        if let Some(r) = self.roots.iter().find(|r| r.settings.path == path) {
            self.draft = r.settings.clone();
            self.selected = Some(Selection::Existing(path.to_path_buf()));
            self.refill = true;
            self.confirm_remove = false;
            cx.notify();
        }
    }

    fn select_new(&mut self, cx: &mut Context<'_, Self>) {
        self.draft = RootSettings::new(PathBuf::new());
        self.selected = Some(Selection::New);
        self.refill = true;
        self.confirm_remove = false;
        self.status = None;
        cx.notify();
    }

    fn refill_inputs(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.refill = false;
        let d = self.draft.clone();
        let path = if d.path.as_os_str().is_empty() {
            String::new()
        } else {
            d.path.display().to_string()
        };
        self.path_in
            .update(cx, |s, cx| s.set_value(path, window, cx));
        self.interval_in.update(cx, |s, cx| {
            s.set_value(d.interval_minutes.to_string(), window, cx)
        });
        self.full_in.update(cx, |s, cx| {
            s.set_value(d.full_every.to_string(), window, cx)
        });
        self.exclude_in
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.contentid_in
            .update(cx, |s, cx| s.set_value("", window, cx));
    }

    fn add_exclude(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let v = self.exclude_in.read(cx).value().trim().to_string();
        if !v.is_empty() && !self.draft.exclude.contains(&v) {
            self.draft.exclude.push(v);
        }
        self.exclude_in
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn add_contentid(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let v = self
            .contentid_in
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .to_string();
        if !v.is_empty() {
            let p = PathBuf::from(v);
            if !self.draft.contentid.contains(&p) {
                self.draft.contentid.push(p);
            }
        }
        self.contentid_in
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// The draft with the scalar inputs folded in, or why it can't be saved.
    fn collect(&self, cx: &Context<'_, Self>) -> Result<RootSettings, String> {
        let mut root = self.draft.clone();
        if self.selected == Some(Selection::New) {
            let p = self.path_in.read(cx).value().trim().to_string();
            if p.is_empty() {
                return Err("enter the directory to index".into());
            }
            root.path = PathBuf::from(p);
        }
        let parse = |s: &Entity<InputState>, what: &str| {
            s.read(cx)
                .value()
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("{what} must be a whole number"))
        };
        root.interval_minutes = parse(&self.interval_in, "rescan interval")?;
        root.full_every = u32::try_from(parse(&self.full_in, "full rescan cadence")?)
            .map_err(|_| "full rescan cadence is too large".to_string())?;
        Ok(root)
    }

    fn save(&mut self, cx: &mut Context<'_, Self>) {
        let root = match self.collect(cx) {
            Ok(r) => r,
            Err(e) => {
                self.status = Some((Status::Error, e));
                cx.notify();
                return;
            }
        };
        self.run(
            Request::PutRoot { root: root.clone() },
            Some(root.path),
            "saved",
            cx,
        );
    }

    fn remove(&mut self, cx: &mut Context<'_, Self>) {
        let Some(Selection::Existing(path)) = self.selected.clone() else {
            return;
        };
        self.confirm_remove = false;
        self.selected = None;
        self.run(Request::RemoveRoot { path }, None, "removed", cx);
    }

    fn run(
        &mut self,
        req: Request,
        then: Option<PathBuf>,
        done: &'static str,
        cx: &mut Context<'_, Self>,
    ) {
        self.busy = true;
        self.status = None;
        let task = cx
            .background_executor()
            .spawn(async move { fetch(move |c| c.request(&req))() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(v) => {
                        let msg = format!("{done}; daemon applied it: {v}");
                        print_info(&msg);
                        this.status = Some((Status::Success, format!("{done} and applied")));
                        if let Some(p) = then {
                            // Pick it up by its canonical path on the next load.
                            this.selected = Some(Selection::Existing(steward_expand(&p)));
                        }
                        cx.emit(SettingsEvent::Changed);
                    }
                    Err(e) => {
                        print_warning(&e);
                        this.status = Some((Status::Error, e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

fn steward_expand(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => std::env::home_dir().unwrap_or_default().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn label(theme: &Theme, text: &str, hint: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .w(rems(16.0))
        .flex_shrink_0()
        .child(div().text_color(theme.bright).child(text.to_string()))
        .child(
            div()
                .text_size(rems(0.6875))
                .text_color(theme.secondary)
                .child(hint.to_string()),
        )
}

fn field(theme: &Theme, text: &str, hint: &str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(rems(1.0))
        .child(label(theme, text, hint))
        .child(control)
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self.refill {
            self.refill_inputs(window, cx);
        }
        let theme = cx.omarchy().clone();

        // Root list.
        let mut list = div()
            .flex()
            .flex_col()
            .w(rems(18.0))
            .flex_shrink_0()
            .gap(rems(0.25))
            .p(rems(0.75))
            .border_r_1()
            .border_color(theme.divider())
            .child(div().text_color(theme.bright).pb(rems(0.25)).child("Roots"));
        for (i, r) in self.roots.iter().enumerate() {
            let path = r.settings.path.clone();
            let selected = self.selected == Some(Selection::Existing(path.clone()));
            let state = match &r.indexed {
                Some(e) => format!(
                    "{} files · {}",
                    format::count(e.total_files),
                    format::bytes(e.total_alloc)
                ),
                None => "not scanned yet".into(),
            };
            list = list.child(
                div()
                    .id(("root-row", i))
                    .flex()
                    .flex_col()
                    .p(rems(0.5))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.selection))
                    .hover(|d| d.bg(theme.surface))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.status = None;
                        this.select_path(&path, cx);
                    }))
                    .child(
                        div()
                            .truncate()
                            .text_color(theme.bright)
                            .child(r.settings.path.display().to_string()),
                    )
                    .child(
                        div()
                            .text_size(rems(0.6875))
                            .text_color(theme.secondary)
                            .child(state),
                    ),
            );
        }
        list = list
            .child(
                div().pt(rems(0.5)).child(
                    button("add-root", "Add root", ButtonVariant::Secondary, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.select_new(cx))),
                ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .child(format!("Saved to {}, comments kept.", self.file)),
            );

        // Form.
        let new = self.selected == Some(Selection::New);
        let d = self.draft.clone();
        let title = if new {
            "New root".to_string()
        } else {
            d.path.display().to_string()
        };
        let mut form = div().flex().flex_col().gap(rems(0.875)).p(rems(1.0)).child(
            div()
                .text_size(rems(1.0))
                .text_color(theme.bright)
                .child(title),
        );

        if self.selected.is_none() {
            form = form.child(
                div()
                    .text_color(theme.secondary)
                    .child("Select a root, or add one."),
            );
        } else {
            if new {
                form = form.child(field(
                    &theme,
                    "Directory",
                    "an existing directory; it becomes part of the one namespace",
                    div()
                        .w(rems(28.0))
                        .child(input("path", &self.path_in, window, cx)),
                ));
            }
            form = form
                .child(field(
                    &theme,
                    "Rescan every (minutes)",
                    "trusting rescans skip directories whose mtime is unchanged",
                    number_input(&self.interval_in, cx),
                ))
                .child(field(
                    &theme,
                    "Full rescan every (rescans)",
                    "stats every file, catching in-place changes; 0 = always",
                    number_input(&self.full_in, cx),
                ))
                .child(field(
                    &theme,
                    "One filesystem",
                    "don't descend into other mounts below this root",
                    switch("one-fs", "", d.one_filesystem, cx).on_change({
                        let view = cx.entity();
                        move |v, _, _, cx| {
                            view.update(cx, |this, cx| {
                                this.draft.one_filesystem = v;
                                cx.notify();
                            })
                        }
                    }),
                ))
                .child(field(
                    &theme,
                    "Classify",
                    "tag repos, ignored build output, caches, trash",
                    switch("classify", "", d.classify, cx).on_change({
                        let view = cx.entity();
                        move |v, _, _, cx| {
                            view.update(cx, |this, cx| {
                                this.draft.classify = v;
                                cx.notify();
                            })
                        }
                    }),
                ));

            // Exclude patterns.
            let mut excludes = div().flex().flex_col().gap(rems(0.25)).w(rems(28.0));
            for (i, pat) in d.exclude.iter().enumerate() {
                excludes = excludes.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px(rems(0.5))
                        .bg(theme.surface)
                        .child(
                            div()
                                .font_family(theme.mono_font.clone())
                                .child(pat.clone()),
                        )
                        .child(
                            button(("rm-ex", i), "Remove", ButtonVariant::Outline, cx).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.draft.exclude.remove(i);
                                    cx.notify();
                                }),
                            ),
                        ),
                );
            }
            excludes = excludes.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rems(0.5))
                    .child(
                        div()
                            .flex_1()
                            .child(input("new-ex", &self.exclude_in, window, cx)),
                    )
                    .child(
                        button("add-ex", "Add", ButtonVariant::Secondary, cx).on_click(
                            cx.listener(|this, _, window, cx| this.add_exclude(window, cx)),
                        ),
                    ),
            );
            form = form.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rems(1.0))
                    .child(label(
                        &theme,
                        "Exclude",
                        "gitignore syntax, relative to the root",
                    ))
                    .child(excludes),
            );

            // Content-id folders.
            let mut cids = div().flex().flex_col().gap(rems(0.25)).w(rems(28.0));
            for (i, c) in d.contentid.iter().enumerate() {
                let shown = c.strip_prefix(&d.path).unwrap_or(c).display().to_string();
                cids = cids.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px(rems(0.5))
                        .bg(theme.surface)
                        .child(shown)
                        .child(
                            button(("rm-cid", i), "Remove", ButtonVariant::Outline, cx).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.draft.contentid.remove(i);
                                    cx.notify();
                                }),
                            ),
                        ),
                );
            }
            cids = cids.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rems(0.5))
                    .child(
                        div()
                            .flex_1()
                            .child(input("new-cid", &self.contentid_in, window, cx)),
                    )
                    .child(
                        button("add-cid", "Add", ButtonVariant::Secondary, cx).on_click(
                            cx.listener(|this, _, window, cx| this.add_contentid(window, cx)),
                        ),
                    ),
            );
            form = form.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rems(1.0))
                    .child(label(
                        &theme,
                        "Content-id folders",
                        "files here get BitTorrent v2 ids; hashing reads every byte",
                    ))
                    .child(cids),
            );

            // Index state.
            if let Some(Selection::Existing(p)) = &self.selected
                && let Some(r) = self.roots.iter().find(|r| &r.settings.path == p)
            {
                let state = match &r.indexed {
                    Some(e) => badge(
                        format!(
                            "indexed: {} dirs, {} files, {} on disk",
                            format::count(e.total_dirs.saturating_sub(1)),
                            format::count(e.total_files),
                            format::bytes(e.total_alloc)
                        ),
                        Status::Success,
                        cx,
                    ),
                    None => badge("not scanned yet", Status::Warning, cx),
                };
                form = form.child(field(&theme, "Index", "", state));
            }

            // Actions.
            let mut actions = div()
                .flex()
                .flex_row()
                .gap(rems(0.5))
                .pt(rems(0.5))
                .child(
                    button(
                        "save",
                        if new { "Add root" } else { "Save" },
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                )
                .child(
                    button("revert", "Revert", ButtonVariant::Secondary, cx).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.status = None;
                            match this.selected.clone() {
                                Some(Selection::Existing(p)) => this.select_path(&p, cx),
                                _ => this.select_new(cx),
                            }
                        },
                    )),
                );
            if !new {
                actions = actions.child(if self.confirm_remove {
                    button(
                        "confirm-rm",
                        "Really remove? Drops it from the index",
                        ButtonVariant::Danger,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.remove(cx)))
                } else {
                    button("rm-root", "Remove root", ButtonVariant::Danger, cx).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.confirm_remove = true;
                            cx.notify();
                        }),
                    )
                });
            }
            form = form.child(actions);
        }
        if let Some((status, msg)) = &self.status {
            form = form.child(alert(msg.clone(), *status, cx));
        }

        div()
            .track_focus(&self.focus)
            .flex()
            .flex_row()
            .size_full()
            .child(list)
            .child(
                div()
                    .id("settings-form")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(form),
            )
    }
}
