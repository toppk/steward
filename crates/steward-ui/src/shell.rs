//! The window: title bar, root switcher and tabs around the three views.
//! Every view is a plain client of stewardd's API, like any other app.

use std::path::{Path, PathBuf};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, KeyBinding,
    KeyDownEvent, ParentElement as _, Render, Styled as _, Subscription, Window, actions, div,
    rems,
};
use gpui_omarchy::{ActiveTheme as _, ButtonVariant, ChoiceItem, button, tab_list, with_tooltip};

use crate::client::{RootInfo, fetch, print_line, settings};
use crate::content::{ContentEvent, ContentView};
use crate::settings::{SettingsEvent, SettingsView};
use crate::tree::{TreeEvent, TreeView};

actions!(steward, [Quit]);

pub fn bind_keys(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Tree,
    Content,
    Settings,
}

const TABS: [(Tab, &str); 3] = [
    (Tab::Tree, "Tree"),
    (Tab::Content, "Content ids"),
    (Tab::Settings, "Settings"),
];

pub struct Shell {
    tab: Tab,
    roots: Vec<RootInfo>,
    scanning: bool,
    hashing: Option<(String, f32)>,
    /// The configured root being explored.
    current: Option<PathBuf>,
    tree: Entity<TreeView>,
    content: Entity<ContentView>,
    settings: Entity<SettingsView>,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    pub fn new(start: PathBuf, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let tree = cx.new(|cx| TreeView::new(start.clone(), window, cx));
        let content = cx.new(|cx| ContentView::new(window, cx));
        let settings = cx.new(|cx| SettingsView::new(window, cx));
        let subs = vec![
            cx.subscribe_in(
                &tree,
                window,
                |this, _, ev: &TreeEvent, window, cx| match ev {
                    TreeEvent::ShowContent(id) => {
                        this.content
                            .update(cx, |c, cx| c.lookup(id.clone(), window, cx));
                        this.switch(Tab::Content, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &content,
                window,
                |this, _, ev: &ContentEvent, window, cx| match ev {
                    ContentEvent::Reveal(path) => {
                        this.tree.update(cx, |t, cx| t.reveal_path(path, cx));
                        this.switch(Tab::Tree, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &settings,
                window,
                |this, _, ev: &SettingsEvent, _, cx| match ev {
                    SettingsEvent::Changed => this.load_roots(None, cx),
                },
            ),
        ];
        let mut this = Self {
            tab: Tab::Tree,
            roots: Vec::new(),
            scanning: false,
            hashing: None,
            current: None,
            tree,
            content,
            settings,
            _subscriptions: subs,
        };
        this.load_roots(Some(start), cx);
        this.poll_status(cx);
        this
    }

    /// Keep the scanning/hashing badges current; they change without any
    /// action in this window.
    fn poll_status(&mut self, cx: &mut Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(10))
                    .await;
                let Ok(s) = cx
                    .background_executor()
                    .spawn(async move { fetch(settings)() })
                    .await
                else {
                    continue;
                };
                let alive = this.update(cx, |this, cx| {
                    this.scanning = s.scanning;
                    this.hashing = s.hashing;
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        let handle = match self.tab {
            Tab::Tree => self.tree.read(cx).focus.clone(),
            Tab::Content => self.content.read(cx).focus.clone(),
            Tab::Settings => self.settings.read(cx).focus.clone(),
        };
        window.focus(&handle, cx);
    }

    /// Fetch the configured roots; on first load pick the one `start` is in.
    fn load_roots(&mut self, start: Option<PathBuf>, cx: &mut Context<'_, Self>) {
        let task = cx
            .background_executor()
            .spawn(async move { fetch(settings)() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(s) => {
                        this.roots = s.roots;
                        this.scanning = s.scanning;
                        this.hashing = s.hashing;
                        let file = s.file;
                        let wanted = start.or_else(|| this.current.clone());
                        this.current = wanted
                            .and_then(|p| {
                                this.roots
                                    .iter()
                                    .filter(|r| p.starts_with(&r.settings.path))
                                    .max_by_key(|r| r.settings.path.components().count())
                                    .map(|r| r.settings.path.clone())
                            })
                            .or_else(|| this.roots.first().map(|r| r.settings.path.clone()));
                        let roots = this.roots.clone();
                        let current = this.current.clone();
                        this.content
                            .update(cx, |c, cx| c.set_root(current.clone(), &roots, cx));
                        this.settings
                            .update(cx, |s, cx| s.set_roots(roots, current, file, cx));
                    }
                    Err(e) => print_line("warning: ", &e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn switch_root(&mut self, path: PathBuf, cx: &mut Context<'_, Self>) {
        self.current = Some(path.clone());
        self.tree.update(cx, |t, cx| t.set_root(path.clone(), cx));
        let roots = self.roots.clone();
        self.content
            .update(cx, |c, cx| c.set_root(Some(path.clone()), &roots, cx));
        self.settings.update(cx, |s, cx| s.select_path(&path, cx));
        cx.notify();
    }

    fn switch(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.tab = tab;
        if tab == Tab::Content {
            self.content.update(cx, |c, cx| c.refresh(cx));
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let ks = &ev.keystroke;
        if zoom(ks, window) {
            cx.notify();
            return;
        }
        if ks.modifiers.control {
            let tab = match ks.key.as_str() {
                "1" => Tab::Tree,
                "2" => Tab::Content,
                "3" => Tab::Settings,
                _ => return,
            };
            self.switch(tab, window, cx);
        }
    }
}

const BASE_REM: f32 = 16.0;
const ZOOM_STEPS: [f32; 9] = [0.625, 0.75, 0.875, 1.0, 1.125, 1.25, 1.5, 1.75, 2.0];
const ZOOM_DEFAULT: usize = 3;

/// Ctrl +/−/0: every size in this UI is in rems, so one rem change scales all.
fn zoom(ks: &gpui_kit::Keystroke, window: &mut Window) -> bool {
    if !ks.modifiers.control {
        return false;
    }
    let current = window.rem_size().as_f32() / BASE_REM;
    let ix = ZOOM_STEPS
        .iter()
        .position(|s| (s - current).abs() < 0.01)
        .unwrap_or(ZOOM_DEFAULT);
    let next = match ks.key.as_str() {
        "=" | "+" | "add" => (ix + 1).min(ZOOM_STEPS.len() - 1),
        "-" | "subtract" => ix.saturating_sub(1),
        "0" => ZOOM_DEFAULT,
        _ => return false,
    };
    window.set_rem_size(gpui_kit::px(BASE_REM * ZOOM_STEPS[next]));
    true
}

fn root_label(path: &Path) -> String {
    let home = std::env::home_dir().unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) if rest.components().count() == 1 => format!("~/{}", rest.display()),
        // Deep paths would crowd out the tabs; the tooltip has the full path.
        _ => path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.omarchy().clone();
        let title = match &self.current {
            Some(p) => format!("steward · {}", root_label(p)),
            None => "steward".to_string(),
        };
        window.set_window_title(&title);

        let mut roots = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .items_center()
            .gap(rems(0.375))
            .child(
                div()
                    .text_color(theme.secondary)
                    .pr(rems(0.25))
                    .child("Root"),
            );
        for (i, r) in self.roots.iter().enumerate() {
            let path = r.settings.path.clone();
            let selected = self.current.as_deref() == Some(path.as_path());
            let variant = if selected {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Secondary
            };
            let tip = match &r.indexed {
                _ if r.offline => format!(
                    "{}: offline (volume not mounted); showing the index as last scanned",
                    path.display()
                ),
                Some(e) => format!(
                    "{}: {} dirs, {} files, {} on disk",
                    path.display(),
                    crate::format::count(e.total_dirs.saturating_sub(1)),
                    crate::format::count(e.total_files),
                    crate::format::bytes(e.total_alloc)
                ),
                None => format!("{}: not scanned yet", path.display()),
            };
            roots = roots.child(with_tooltip(
                button(
                    ("root", i),
                    if r.offline {
                        format!("{} (offline)", root_label(&path))
                    } else {
                        root_label(&path)
                    },
                    variant,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.switch_root(path.clone(), cx))),
                tip,
            ));
        }
        if self.roots.is_empty() {
            roots = roots.child(div().text_color(theme.warning).child("none configured"));
        }

        let selected = TABS.iter().position(|(t, _)| *t == self.tab);
        let shell = cx.entity();
        let tabs = tab_list(
            "tabs",
            TABS.iter().map(|(_, l)| ChoiceItem::new(*l, *l)).collect(),
            selected,
            false,
            move |i, window, cx| {
                shell.update(cx, |this, cx| this.switch(TABS[i].0, window, cx));
            },
            window,
            cx,
        );

        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(rems(1.0))
            .px(rems(0.75))
            .py(rems(0.375))
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.divider())
            .child(roots)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .gap(rems(0.75))
                    .when_some(self.hashing.clone(), |d, (path, pct)| {
                        let name = Path::new(&path)
                            .file_name()
                            .map_or(path.clone(), |n| n.to_string_lossy().into_owned());
                        d.child(with_tooltip(
                            div().id("hashing").child(gpui_omarchy::badge(
                                format!("hashing {name} {pct:.1}%"),
                                gpui_omarchy::Status::Neutral,
                                cx,
                            )),
                            format!("computing content ids under {path}"),
                        ))
                    })
                    .when(self.scanning, |d| {
                        d.child(gpui_omarchy::badge(
                            "daemon scanning",
                            gpui_omarchy::Status::Warning,
                            cx,
                        ))
                    })
                    .child(div().w(rems(22.0)).child(tabs)),
            );

        let active = match self.tab {
            Tab::Tree => self.tree.clone().into_any_element(),
            Tab::Content => self.content.clone().into_any_element(),
            Tab::Settings => self.settings.clone().into_any_element(),
        };
        let content = div()
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
            .child(bar)
            .child(div().flex_1().min_h_0().child(active))
            .child(
                div()
                    .px(rems(0.75))
                    .py(rems(0.125))
                    .bg(theme.surface)
                    .text_size(rems(0.6875))
                    .text_color(theme.secondary)
                    .when(true, |d| {
                        d.child("ctrl 1/2/3 tabs · ctrl +/−/0 zoom · ctrl-q quit")
                    }),
            );
        crate::decorations::frame(content, title.into(), window, &theme)
    }
}
