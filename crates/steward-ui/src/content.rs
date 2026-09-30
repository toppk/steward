//! Content ids for the current root: coverage per content-id folder,
//! duplicate groups, and lookup of any `btv2:` id.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, rems,
};
use gpui_omarchy::{
    ActiveTheme as _, ButtonVariant, Status, Theme, alert, badge, button, empty_state, input,
    progress, with_tooltip,
};
use serde_json::Value;
use steward_proto::Request;

use crate::client::{RootInfo, fetch, print_line};
use crate::format;

pub enum ContentEvent {
    /// Show this path in the tree.
    Reveal(String),
}

impl EventEmitter<ContentEvent> for ContentView {}

type Fetched<T> = Result<T, String>;

struct Dup {
    id: String,
    size: u64,
    wasted: u64,
    paths: Vec<String>,
}

pub struct ContentView {
    pub focus: FocusHandle,
    root: Option<RootInfo>,
    summaries: Vec<(PathBuf, Option<Result<Value, String>>)>,
    dups: Option<Result<Vec<Dup>, String>>,
    expanded: HashSet<usize>,
    hashing: HashSet<PathBuf>,
    lookup_input: Entity<InputState>,
    /// The id looked up, and its locations once the daemon answers.
    lookup: Option<(String, Option<Fetched<Vec<String>>>)>,
    _subscriptions: Vec<Subscription>,
}

impl ContentView {
    pub fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let lookup_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("btv2:… content id to look up"));
        let sub = cx.subscribe_in(
            &lookup_input,
            window,
            |this, s, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    let id = s.read(cx).value().trim().to_string();
                    this.lookup(id, window, cx);
                }
            },
        );
        Self {
            focus: cx.focus_handle(),
            root: None,
            summaries: Vec::new(),
            dups: None,
            expanded: HashSet::new(),
            hashing: HashSet::new(),
            lookup_input,
            lookup: None,
            _subscriptions: vec![sub],
        }
    }

    pub fn set_root(
        &mut self,
        path: Option<PathBuf>,
        roots: &[RootInfo],
        cx: &mut Context<'_, Self>,
    ) {
        let info = path.and_then(|p| roots.iter().find(|r| r.settings.path == p).cloned());
        let changed = info.as_ref().map(|r| &r.settings) != self.root.as_ref().map(|r| &r.settings);
        self.root = info;
        if changed {
            self.expanded.clear();
            self.refresh(cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        let Some(root) = self.root.clone() else {
            self.summaries.clear();
            self.dups = None;
            cx.notify();
            return;
        };
        let folders = root.settings.contentid.clone();
        self.summaries = folders.iter().map(|f| (f.clone(), None)).collect();
        self.dups = None;
        let task = cx.background_executor().spawn(async move {
            fetch(move |c| {
                let mut sums = Vec::new();
                for f in &folders {
                    sums.push(
                        c.request(&Request::ContentSummary { path: f.clone() })
                            .map_err(|e| e.0),
                    );
                }
                let dups = c
                    .request(&Request::Duplicates {
                        path: root.settings.path.clone(),
                        limit: 500,
                    })
                    .map_err(|e| e.0)
                    .map(|v| {
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|d| Dup {
                                id: d["id"].as_str().unwrap_or_default().to_string(),
                                size: d["size"].as_u64().unwrap_or(0),
                                wasted: d["wasted"].as_u64().unwrap_or(0),
                                paths: d["paths"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|p| p.as_str().map(String::from))
                                    .collect(),
                            })
                            .collect()
                    });
                Ok((sums, dups))
            })()
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((sums, dups)) => {
                        for (slot, s) in this.summaries.iter_mut().zip(sums) {
                            slot.1 = Some(s);
                        }
                        this.dups = Some(dups);
                    }
                    Err(e) => {
                        print_line("warning: ", &e);
                        this.dups = Some(Err(e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn hash_now(&mut self, folder: PathBuf, cx: &mut Context<'_, Self>) {
        if !self.hashing.insert(folder.clone()) {
            return;
        }
        let path = folder.clone();
        let task = cx
            .background_executor()
            .spawn(async move { fetch(move |c| c.request(&Request::HashTree { path }))() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.hashing.remove(&folder);
                match result {
                    Ok(v) => print_line("", &format!("hashed {}: {v}", folder.display())),
                    Err(e) => print_line("warning: ", &e),
                }
                this.refresh(cx);
            });
        })
        .detach();
        cx.notify();
    }

    pub fn lookup(&mut self, id: String, window: &mut Window, cx: &mut Context<'_, Self>) {
        if id.is_empty() {
            return;
        }
        self.lookup_input
            .update(cx, |s, cx| s.set_value(id.clone(), window, cx));
        self.lookup = Some((id.clone(), None));
        let task = cx.background_executor().spawn({
            let id = id.clone();
            async move {
                fetch(move |c| {
                    let v = c.request(&Request::FindContent { id })?;
                    Ok(serde_json::from_value::<Vec<String>>(v).unwrap_or_default())
                })()
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = &result {
                    print_line("warning: ", e);
                }
                if this.lookup.as_ref().is_some_and(|(i, _)| *i == id) {
                    this.lookup = Some((id, Some(result)));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

fn section(title: &str, theme: &Theme) -> Div {
    div().flex().flex_col().gap(rems(0.5)).child(
        div()
            .text_color(theme.bright)
            .text_size(rems(0.9375))
            .child(title.to_string()),
    )
}

fn path_row(
    id: impl Into<gpui_kit::ElementId>,
    path: String,
    theme: &Theme,
    cx: &mut Context<'_, ContentView>,
) -> impl IntoElement {
    let target = path.clone();
    with_tooltip(
        div()
            .id(id)
            .pl(rems(1.0))
            .truncate()
            .text_color(theme.foreground)
            .hover(|d| d.text_color(theme.accent))
            .cursor_pointer()
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cx.emit(ContentEvent::Reveal(target.clone()));
            }))
            .child(path.clone()),
        format!("{path}\nclick to show in the tree"),
    )
}

impl Render for ContentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.omarchy().clone();
        let mut body = div().flex().flex_col().gap(rems(1.25)).p(rems(1.0));

        let Some(root) = self.root.clone() else {
            return div()
                .track_focus(&self.focus)
                .size_full()
                .child(empty_state(
                    "No root selected",
                    "Configure a root in Settings.",
                    cx,
                ))
                .into_any_element();
        };

        // Coverage per content-id folder.
        let mut folders = section("Content-id folders", &theme);
        if root.settings.contentid.is_empty() {
            folders = folders.child(empty_state(
                "No content-id folders on this root",
                "Add folders under this root in Settings to compute BitTorrent v2 content ids \
                 for their files.",
                cx,
            ));
        }
        for (i, (folder, sum)) in self.summaries.iter().enumerate() {
            let hashing = self.hashing.contains(folder);
            let target = folder.clone();
            let mut card = div()
                .flex()
                .flex_col()
                .gap(rems(0.375))
                .p(rems(0.75))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_color(theme.bright)
                                .child(folder.display().to_string()),
                        )
                        .child(
                            button(
                                ("hash", i),
                                if hashing { "Hashing…" } else { "Hash now" },
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .disabled(hashing)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.hash_now(target.clone(), cx);
                                },
                            )),
                        ),
                );
            card = match sum {
                None => card.child(div().text_color(theme.secondary).child("loading…")),
                Some(Err(e)) => card.child(alert(e.clone(), Status::Error, cx)),
                Some(Ok(s)) => {
                    let n = |k: &str| s[k].as_u64().unwrap_or(0);
                    let pct = if n("bytes") == 0 {
                        100.0
                    } else {
                        n("hashed_bytes") as f32 * 100.0 / n("bytes") as f32
                    };
                    card.child(progress(("cover", i), pct, cx))
                        .child(format!(
                            "{} of {} files hashed ({} of {})",
                            format::count(n("hashed_files")),
                            format::count(n("files")),
                            format::bytes(n("hashed_bytes")),
                            format::bytes(n("bytes")),
                        ))
                        .child(div().text_color(theme.secondary).child(format!(
                            "{} distinct ids · {} duplicate groups ({} files) · {} reclaimable",
                            format::count(n("distinct_ids")),
                            format::count(n("duplicate_groups")),
                            format::count(n("duplicate_files")),
                            format::bytes(n("wasted_bytes")),
                        )))
                        .when(s["truncated"].as_bool().unwrap_or(false), |d| {
                            d.child(badge("stopped at 2,000,000 files", Status::Warning, cx))
                        })
                }
            };
            folders = folders.child(card);
        }
        body = body.child(folders);

        // Lookup.
        let mut lookup =
            section("Look up a content id", &theme).child(
                div()
                    .flex()
                    .flex_row()
                    .gap(rems(0.5))
                    .child(
                        div()
                            .flex_1()
                            .child(input("lookup", &self.lookup_input, window, cx)),
                    )
                    .child(button("find", "Find", ButtonVariant::Primary, cx).on_click(
                        cx.listener(|this, _, window, cx| {
                            let id = this.lookup_input.read(cx).value().trim().to_string();
                            this.lookup(id, window, cx);
                        }),
                    )),
            );
        if let Some((id, result)) = &self.lookup {
            lookup = match result {
                None => lookup.child(div().text_color(theme.secondary).child("looking up…")),
                Some(Err(e)) => lookup.child(alert(e.clone(), Status::Error, cx)),
                Some(Ok(paths)) if paths.is_empty() => lookup.child(alert(
                    format!("No indexed file currently has {id}"),
                    Status::Warning,
                    cx,
                )),
                Some(Ok(paths)) => {
                    let mut l = lookup.child(
                        div()
                            .text_color(theme.secondary)
                            .child(format!("{} locations", paths.len())),
                    );
                    for (j, p) in paths.iter().enumerate() {
                        l = l.child(path_row(("hit", j), p.clone(), &theme, cx));
                    }
                    l
                }
            };
        }
        body = body.child(lookup);

        // Duplicates.
        let mut dups = section(
            &format!("Duplicate content under {}", root.settings.path.display()),
            &theme,
        );
        dups = match &self.dups {
            None => dups.child(div().text_color(theme.secondary).child("loading…")),
            Some(Err(e)) => dups.child(alert(e.clone(), Status::Error, cx)),
            Some(Ok(list)) if list.is_empty() => dups.child(
                div()
                    .text_color(theme.secondary)
                    .child("No content stored at more than one path (among hashed files)."),
            ),
            Some(Ok(list)) => {
                let mut d = dups;
                for (i, g) in list.iter().enumerate() {
                    let open = self.expanded.contains(&i);
                    let short: SharedString = format!("{}…", &g.id[..g.id.len().min(21)]).into();
                    d = d.child(
                        div()
                            .id(("dup", i))
                            .flex()
                            .flex_row()
                            .gap(rems(1.0))
                            .px(rems(0.5))
                            .py(rems(0.25))
                            .hover(|d| d.bg(theme.surface))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded.remove(&i) {
                                    this.expanded.insert(i);
                                }
                                cx.notify();
                            }))
                            .child(div().w(rems(1.0)).child(if open { "▾" } else { "▸" }))
                            .child(div().w(rems(7.0)).child(format::bytes(g.size)))
                            .child(
                                div()
                                    .w(rems(5.0))
                                    .child(format!("{} copies", g.paths.len())),
                            )
                            .child(
                                div()
                                    .w(rems(9.0))
                                    .text_color(theme.warning)
                                    .child(format!("{} wasted", format::bytes(g.wasted))),
                            )
                            .child(with_tooltip(
                                div()
                                    .id(("dupid", i))
                                    .text_color(theme.secondary)
                                    .child(short),
                                g.id.clone(),
                            )),
                    );
                    if open {
                        for (j, p) in g.paths.iter().enumerate() {
                            d = d.child(path_row(
                                ("dupath", i * 10_000 + j),
                                p.clone(),
                                &theme,
                                cx,
                            ));
                        }
                    }
                }
                d
            }
        };
        body = body.child(dups);

        div()
            .id("content-view")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .px(rems(1.0))
                    .pt(rems(0.75))
                    .child(
                        button("refresh", "Refresh", ButtonVariant::Secondary, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(body)
            .into_any_element()
    }
}
