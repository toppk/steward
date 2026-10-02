//! The daemon's internal state, as a snapshot: what it is scanning and
//! hashing right now, its roots and schedule, recent scans, warnings and
//! errors, clients and events. Refreshed on demand, or every two seconds.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, rems,
};
use gpui_omarchy::{
    ActiveTheme as _, ButtonVariant, Status, Theme, alert, badge, button, progress, switch,
    with_tooltip,
};
use serde_json::Value;

use crate::client::{Snapshot, fetch, print_warning, snapshot};
use crate::format;

const AUTO_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
/// Rows shown per list before "… N more".
const ROWS: usize = 25;

pub struct DaemonView {
    pub focus: FocusHandle,
    snap: Option<Result<Snapshot, String>>,
    loading: bool,
    auto: bool,
    /// Bumped when auto-refresh stops, so the old loop ends.
    auto_gen: u64,
    show_all_events: bool,
    show_all_problems: bool,
}

impl DaemonView {
    pub fn new(_window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            snap: None,
            loading: false,
            auto: false,
            auto_gen: 0,
            show_all_events: false,
            show_all_problems: false,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let task = cx
            .background_executor()
            .spawn(async move { fetch(snapshot)() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = &result {
                    print_warning(e);
                }
                this.snap = Some(result);
                this.loading = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_auto(&mut self, on: bool, cx: &mut Context<'_, Self>) {
        self.auto = on;
        self.auto_gen += 1;
        if on {
            let generation = self.auto_gen;
            self.refresh(cx);
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(AUTO_EVERY).await;
                    let go_on = this.update(cx, |this, cx| {
                        let current = this.auto && this.auto_gen == generation;
                        if current {
                            this.refresh(cx);
                        }
                        current
                    });
                    if !matches!(go_on, Ok(true)) {
                        return;
                    }
                }
            })
            .detach();
        }
        cx.notify();
    }
}

fn section(title: &str, theme: &Theme) -> Div {
    div().flex().flex_col().gap(rems(0.375)).child(
        div()
            .text_color(theme.bright)
            .text_size(rems(0.9375))
            .child(title.to_string()),
    )
}

fn card(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(rems(0.25))
        .p(rems(0.75))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
}

/// `label: value` on one line, label dimmed.
fn field(label: &str, value: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_row()
        .gap(rems(0.5))
        .child(
            div()
                .w(rems(9.0))
                .flex_shrink_0()
                .text_color(theme.secondary)
                .child(label.to_string()),
        )
        .child(div().min_w_0().truncate().child(value.into()))
}

fn dim(text: impl Into<SharedString>, theme: &Theme) -> Div {
    div().text_color(theme.secondary).child(text.into())
}

/// A truncated path with the whole of it in a tooltip.
fn path_cell(id: impl Into<gpui_kit::ElementId>, path: &str, width: f32) -> impl IntoElement {
    with_tooltip(
        div()
            .id(id)
            .w(rems(width))
            .flex_shrink_0()
            .truncate()
            .child(path.to_string()),
        path.to_string(),
    )
}

fn row() -> Div {
    div().flex().flex_row().gap(rems(0.75)).items_center()
}

fn n(v: &Value, k: &str) -> u64 {
    v[k].as_u64().unwrap_or(0)
}

fn f(v: &Value, k: &str) -> f64 {
    v[k].as_f64().unwrap_or(0.0)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_default()
}

fn pct_of(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        100.0
    } else {
        (part as f32 * 100.0 / whole as f32).min(100.0)
    }
}

fn short_id(id: &str) -> String {
    match id.strip_prefix("btv2:") {
        Some(hex) if hex.len() > 12 => format!("btv2:{}…", &hex[..12]),
        _ => id.to_string(),
    }
}

impl DaemonView {
    fn daemon_card(&self, snap: &Snapshot, theme: &Theme) -> Div {
        let d = &snap.status["daemon"];
        let mut c = card(theme);
        if d.is_null() {
            return c.child(dim(
                "This stewardd is older than the Daemon tab; restart it for details.",
                theme,
            ));
        }
        c = c
            .child(field(
                "stewardd",
                format!("version {} · pid {}", s(d, "version"), n(d, "pid")),
                theme,
            ))
            .child(field(
                "Running",
                format!(
                    "{} (since {})",
                    format::duration(f(d, "uptime_secs")),
                    format::clock(f(d, "started"))
                ),
                theme,
            ))
            .child(field(
                "Index",
                format!("{} ({})", s(d, "db"), format::bytes(n(d, "db_bytes"))),
                theme,
            ))
            .child(field(
                "Hashing threads",
                n(d, "hash_threads").to_string(),
                theme,
            ))
            .child(field("Admin socket", s(d, "api_socket").to_string(), theme))
            .child(field(
                "Content socket",
                s(d, "content_socket").to_string(),
                theme,
            ));
        let a = &snap.status["activity"];
        c.child(field(
            "Clients",
            format!(
                "{} connected, {} subscribed to events; last event #{}",
                n(a, "connections"),
                n(a, "subscribers"),
                n(a, "event_seq")
            ),
            theme,
        ))
    }

    fn now_card(&self, snap: &Snapshot, theme: &Theme, cx: &mut Context<'_, Self>) -> Div {
        let a = &snap.status["activity"];
        let mut c = card(theme);
        // The scan in progress.
        c = match a.get("scan").filter(|v| !v.is_null()) {
            Some(scan) => c.child(field(
                "Scanning",
                format!(
                    "{} ({} scan, running {})",
                    s(scan, "path"),
                    s(scan, "kind"),
                    format::duration(f(scan, "secs"))
                ),
                theme,
            )),
            None if snap.status["scanning"].as_bool() == Some(true) => {
                c.child(field("Scanning", "yes", theme))
            }
            None => c.child(field("Scanning", "idle", theme)),
        };
        // The hashing job.
        let h = &snap.status["hashing"];
        if h.is_null() {
            c = c.child(field("Hashing", "idle", theme));
        } else {
            // The daemon counts what it has read of files in flight.
            let (done, total) = (n(h, "bytes_done"), n(h, "bytes_total"));
            let pct = pct_of(done, total);
            let started = f(h, "started");
            let elapsed = if started > 0.0 {
                snap.taken - started
            } else {
                0.0
            };
            let rate = if elapsed > 1.0 {
                done as f64 / elapsed
            } else {
                0.0
            };
            let eta = if rate > 0.0 {
                format!(
                    ", about {} left",
                    format::duration((total - done) as f64 / rate)
                )
            } else {
                String::new()
            };
            c = c
                .child(field(
                    "Hashing",
                    format!(
                        "{}: {} of {} files, {} of {}",
                        s(h, "path"),
                        format::count(n(h, "files_done")),
                        format::count(n(h, "files_total")),
                        format::bytes(done),
                        format::bytes(total)
                    ),
                    theme,
                ))
                .child(progress("hash-progress", pct, cx))
                .when(started > 0.0, |d| {
                    d.child(dim(
                        format!(
                            "{pct:.1}% · started {} · {}/s{eta}",
                            format::clock(started),
                            format::bytes(rate as u64)
                        ),
                        theme,
                    ))
                });
        }
        // Files being read right now.
        let reading = a["reading"].as_array().cloned().unwrap_or_default();
        c = c.child(field(
            "Reading now",
            if reading.is_empty() {
                "nothing".to_string()
            } else {
                format!("{} files", reading.len())
            },
            theme,
        ));
        for (i, r) in reading.iter().enumerate() {
            c = c.child(
                row()
                    .pl(rems(1.0))
                    .child(path_cell(("reading", i), s(r, "path"), 40.0))
                    .child(div().w(rems(14.0)).child(format!(
                        "{} of {}",
                        format::bytes(n(r, "read")),
                        format::bytes(n(r, "size"))
                    )))
                    .child(div().w(rems(6.0)).child(progress(
                        ("read", i),
                        pct_of(n(r, "read"), n(r, "size")),
                        cx,
                    )))
                    .child(dim(
                        format!("for {}", format::duration(f(r, "secs"))),
                        theme,
                    )),
            );
        }
        let queue = a["hash_queue"].as_array().cloned().unwrap_or_default();
        c = c.child(field(
            "Hash queue",
            if queue.is_empty() {
                "empty".to_string()
            } else {
                format!("{} folders waiting", queue.len())
            },
            theme,
        ));
        for (i, q) in queue.iter().enumerate() {
            c = c.child(row().pl(rems(1.0)).child(path_cell(
                ("queued", i),
                q.as_str().unwrap_or_default(),
                48.0,
            )));
        }
        c
    }

    fn roots_card(&self, snap: &Snapshot, theme: &Theme, cx: &mut Context<'_, Self>) -> Div {
        let mut c = card(theme);
        let schedule = snap.status["schedule"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let roots = snap
            .settings
            .as_ref()
            .and_then(|s| s["roots"].as_array().cloned())
            .unwrap_or_default();
        if roots.is_empty() {
            return c.child(dim("No roots configured.", theme));
        }
        c = c.child(
            row()
                .text_color(theme.secondary)
                .child(div().w(rems(22.0)).child("root"))
                .child(div().w(rems(7.0)).child("state"))
                .child(div().w(rems(16.0)).child("indexed"))
                .child(div().w(rems(9.0)).child("rescans"))
                .child(div().w(rems(16.0)).child("next scan"))
                .child(div().child("filesystem")),
        );
        for (i, r) in roots.iter().enumerate() {
            let st = &r["settings"];
            let path = s(st, "path");
            let indexed = &r["indexed"];
            let state = if r["offline"].as_bool() == Some(true) {
                badge("offline", Status::Warning, cx).into_any_element()
            } else if indexed.is_null() {
                badge("not scanned", Status::Neutral, cx).into_any_element()
            } else {
                badge("indexed", Status::Success, cx).into_any_element()
            };
            let totals = if indexed.is_null() {
                String::new()
            } else {
                format!(
                    "{} files · {} items · {}",
                    format::count(n(indexed, "total_files")),
                    format::count(n(indexed, "total_items")),
                    format::bytes(n(indexed, "total_alloc"))
                )
            };
            let every = n(st, "interval_minutes");
            let next = schedule
                .iter()
                .find(|e| s(e, "path") == path)
                .map(|e| f(e, "next"));
            let next = match next {
                Some(t) if t > snap.taken => format!(
                    "{} (in {})",
                    format::clock(t),
                    format::duration(t - snap.taken)
                ),
                Some(_) => "due".to_string(),
                None => "not scheduled".to_string(),
            };
            c = c.child(
                row()
                    .child(path_cell(("root", i), path, 22.0))
                    .child(div().w(rems(7.0)).flex().child(state))
                    .child(div().w(rems(16.0)).child(totals))
                    .child(
                        div()
                            .w(rems(9.0))
                            .child(format!("every {}", format::duration(every as f64 * 60.0))),
                    )
                    .child(dim(next, theme).w(rems(16.0)))
                    .child(match format::filesystem(&r["fs"]) {
                        Some((text, true)) => div().text_color(theme.warning).child(text),
                        Some((text, false)) => div().child(text),
                        None => dim("unknown", theme),
                    }),
            );
        }
        c
    }

    fn scans_card(&self, snap: &Snapshot, theme: &Theme) -> Div {
        let mut c = card(theme);
        let scans = snap.status["recent_scans"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if scans.is_empty() {
            return c.child(dim("No scans since the daemon started.", theme));
        }
        c = c.child(
            row()
                .text_color(theme.secondary)
                .child(div().w(rems(6.0)).child("finished"))
                .child(div().w(rems(24.0)).child("path"))
                .child(div().w(rems(5.0)).child("kind"))
                .child(div().w(rems(5.5)).child("took"))
                .child(div().w(rems(7.0)).child("entries"))
                .child(div().child("changes")),
        );
        for (i, sc) in scans.iter().take(ROWS).enumerate() {
            let mut r = row()
                .child(div().w(rems(6.0)).child(format::clock(f(sc, "finished"))))
                .child(path_cell(("scan", i), s(sc, "root"), 24.0))
                .child(div().w(rems(5.0)).child(s(sc, "kind").to_string()));
            r = match sc.get("error").and_then(Value::as_str) {
                Some(e) => r.child(
                    div()
                        .text_color(theme.danger)
                        .truncate()
                        .child(e.to_string()),
                ),
                None => {
                    let mut changes = format!(
                        "+{} ~{} -{}",
                        format::count(n(sc, "inserted")),
                        format::count(n(sc, "updated")),
                        format::count(n(sc, "deleted"))
                    );
                    if n(sc, "errors") > 0 {
                        changes.push_str(&format!(", {} unreadable", n(sc, "errors")));
                    }
                    let offline = sc["offline"].as_array().map_or(0, Vec::len);
                    if offline > 0 {
                        changes.push_str(&format!(", {offline} offline"));
                    }
                    r.child(
                        div()
                            .w(rems(5.5))
                            .child(format::duration(n(sc, "millis") as f64 / 1000.0)),
                    )
                    .child(
                        div()
                            .w(rems(7.0))
                            .child(format::count(n(sc, "entries_seen"))),
                    )
                    .child(div().child(changes))
                }
            };
            c = c.child(r);
        }
        if scans.len() > ROWS {
            c = c.child(dim(format!("… {} older", scans.len() - ROWS), theme));
        }
        c
    }

    fn problems_card(&self, snap: &Snapshot, theme: &Theme, cx: &mut Context<'_, Self>) -> Div {
        let mut c = card(theme);
        let problems = snap.status["problems"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if problems.is_empty() {
            return c.child(dim("None since the daemon started.", theme));
        }
        let shown = if self.show_all_problems {
            problems.len()
        } else {
            ROWS.min(problems.len())
        };
        for (i, p) in problems.iter().take(shown).enumerate() {
            let error = s(p, "level") == "error";
            let context = s(p, "context");
            let text = if context.is_empty() {
                s(p, "message").to_string()
            } else {
                format!("{context}: {}", s(p, "message"))
            };
            c = c.child(
                row()
                    .child(
                        div()
                            .w(rems(6.0))
                            .flex_shrink_0()
                            .child(format::clock(f(p, "time"))),
                    )
                    .child(div().w(rems(6.0)).flex_shrink_0().child(badge(
                        if error { "error" } else { "warning" },
                        if error {
                            Status::Error
                        } else {
                            Status::Warning
                        },
                        cx,
                    )))
                    .child(with_tooltip(
                        div()
                            .id(("problem", i))
                            .min_w_0()
                            .truncate()
                            .child(text.clone()),
                        text,
                    )),
            );
        }
        if problems.len() > shown {
            c = c.child(
                button(
                    "more-problems",
                    format!("Show all {}", problems.len()),
                    ButtonVariant::Outline,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_all_problems = true;
                    cx.notify();
                })),
            );
        }
        c
    }

    fn events_card(&self, snap: &Snapshot, theme: &Theme, cx: &mut Context<'_, Self>) -> Div {
        let mut c = card(theme);
        if snap.events.is_empty() {
            return c.child(dim(
                "No content or storage events since the daemon started.",
                theme,
            ));
        }
        let shown = if self.show_all_events {
            snap.events.len()
        } else {
            ROWS.min(snap.events.len())
        };
        for (i, e) in snap.events.iter().take(shown).enumerate() {
            let data = &e["data"];
            let what = match s(e, "name") {
                "content.moved" => format!("{} → {}", s(data, "from"), s(data, "to")),
                "content.lost" => format!("{} ({})", s(data, "path"), s(data, "reason")),
                _ => s(data, "path").to_string(),
            };
            let id = data["id"].as_str().map(short_id).unwrap_or_default();
            c = c.child(
                row()
                    .child(
                        div()
                            .w(rems(6.0))
                            .flex_shrink_0()
                            .child(format::clock(f(e, "time"))),
                    )
                    .child(
                        dim(format!("#{}", n(e, "seq")), theme)
                            .w(rems(4.0))
                            .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .w(rems(10.0))
                            .flex_shrink_0()
                            .child(s(e, "name").to_string()),
                    )
                    .child(dim(id, theme).w(rems(11.0)).flex_shrink_0())
                    .child(with_tooltip(
                        div()
                            .id(("event", i))
                            .min_w_0()
                            .truncate()
                            .child(what.clone()),
                        what,
                    )),
            );
        }
        if snap.events.len() > shown {
            c = c.child(
                button(
                    "more-events",
                    format!("Show all {}", snap.events.len()),
                    ButtonVariant::Outline,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_all_events = true;
                    cx.notify();
                })),
            );
        }
        c
    }
}

impl Render for DaemonView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.omarchy().clone();
        let taken = match &self.snap {
            Some(Ok(s)) => format!(
                "Snapshot of {} ({} ago)",
                format::clock(s.taken),
                format::duration(format::now() - s.taken)
            ),
            _ if self.loading => "Loading…".to_string(),
            _ => String::new(),
        };
        let toolbar = row()
            .justify_between()
            .px(rems(1.0))
            .pt(rems(0.75))
            .child(dim(taken, &theme))
            .child(
                row()
                    .child(
                        switch("auto-refresh", "Refresh every 2 s", self.auto, cx).on_change({
                            let view = cx.entity();
                            move |v, _, _, cx| view.update(cx, |this, cx| this.set_auto(v, cx))
                        }),
                    )
                    .child(
                        button(
                            "refresh",
                            if self.loading {
                                "Refreshing…"
                            } else {
                                "Refresh"
                            },
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(self.loading)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            );

        let mut body = div().flex().flex_col().gap(rems(1.25)).p(rems(1.0));
        let snap = self.snap.take();
        match &snap {
            None => body = body.child(dim("No snapshot yet.", &theme)),
            Some(Err(e)) => {
                body = body.child(alert(
                    format!("Could not reach stewardd: {e}"),
                    Status::Error,
                    cx,
                ));
            }
            Some(Ok(s)) => {
                let problems = s.status["problems"].as_array().map_or(0, Vec::len);
                body = body
                    .child(section("Daemon", &theme).child(self.daemon_card(s, &theme)))
                    .child(section("Now", &theme).child(self.now_card(s, &theme, cx)))
                    .child(
                        section(
                            &format!("Warnings and errors ({problems}, newest first)"),
                            &theme,
                        )
                        .child(self.problems_card(s, &theme, cx)),
                    )
                    .child(section("Roots", &theme).child(self.roots_card(s, &theme, cx)))
                    .child(
                        section("Recent scans (newest first)", &theme)
                            .child(self.scans_card(s, &theme)),
                    )
                    .child(
                        section("Recent events (newest first)", &theme)
                            .child(self.events_card(s, &theme, cx)),
                    );
            }
        }
        self.snap = snap;

        div()
            .id("daemon-view")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .child(toolbar)
            .child(body)
    }
}
