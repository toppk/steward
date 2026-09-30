//! Client-side window decorations for compositors that don't draw any
//! (GNOME on Wayland): a title bar with window buttons, and resize edges.
//! Where the compositor decorates (KDE, X11 window managers) this is a no-op.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Bounds, CursorStyle, Decorations, HitboxBehavior, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Pixels, Point, ResizeEdge, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, point, px, rems,
    transparent_black,
};
use gpui_omarchy::Theme;

/// Width of the invisible grab zone around the window.
const INSET: Pixels = px(8.0);

pub fn frame(
    content: impl IntoElement,
    title: SharedString,
    window: &mut Window,
    theme: &Theme,
) -> AnyElement {
    let Decorations::Client { tiling } = window.window_decorations() else {
        window.set_client_inset(px(0.0));
        return content.into_any_element();
    };
    window.set_client_inset(INSET);
    let maximized = window.is_maximized();

    let titlebar = div()
        .id("titlebar")
        .flex()
        .flex_row()
        .items_center()
        .h(rems(2.0))
        .pl(rems(0.75))
        .bg(theme.surface)
        .border_b_1()
        .border_color(theme.divider())
        .on_mouse_down(MouseButton::Left, |e, window, _| {
            if e.click_count >= 2 {
                window.zoom_window();
            } else {
                window.start_window_move();
            }
        })
        .on_mouse_down(MouseButton::Right, |e, window, _| {
            window.show_window_menu(e.position)
        })
        .child(
            div()
                .flex_1()
                .truncate()
                .text_color(theme.secondary)
                .child(title),
        )
        .child(control("minimize", "—", theme, |window, _| {
            window.minimize_window()
        }))
        .child(control(
            "maximize",
            if maximized { "❐" } else { "☐" },
            theme,
            |window, _| {
                window.zoom_window();
            },
        ))
        .child(control("close", "✕", theme, |_, cx| cx.quit()));

    let body = div()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .cursor(CursorStyle::Arrow)
        .border_color(theme.border)
        .when(!tiling.top, |d| d.border_t_1())
        .when(!tiling.bottom, |d| d.border_b_1())
        .when(!tiling.left, |d| d.border_l_1())
        .when(!tiling.right, |d| d.border_r_1())
        .on_mouse_move(|_, _, cx| cx.stop_propagation())
        .child(titlebar)
        .child(div().flex_1().min_h_0().child(content));

    div()
        .id("window-frame")
        .size_full()
        .bg(transparent_black())
        .when(!tiling.top, |d| d.pt(INSET))
        .when(!tiling.bottom, |d| d.pb(INSET))
        .when(!tiling.left, |d| d.pl(INSET))
        .when(!tiling.right, |d| d.pr(INSET))
        .child(
            canvas(
                |_, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    window.insert_hitbox(
                        Bounds::new(point(px(0.), px(0.)), size),
                        HitboxBehavior::Normal,
                    )
                },
                |_, hitbox, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    if let Some(edge) = resize_edge(window.mouse_position(), size) {
                        window.set_cursor_style(cursor(edge), &hitbox);
                    }
                },
            )
            .size_full()
            .absolute(),
        )
        .on_mouse_move(|_, window, _| window.refresh())
        .on_mouse_down(MouseButton::Left, |e, window, _| {
            let size = window.window_bounds().get_bounds().size;
            if let Some(edge) = resize_edge(e.position, size) {
                window.start_window_resize(edge);
            }
        })
        .child(body)
        .into_any_element()
}

fn control(
    id: &'static str,
    glyph: &'static str,
    theme: &Theme,
    action: impl Fn(&mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .h_full()
        .w(rems(2.75))
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.secondary)
        .hover(|d| d.bg(theme.selection))
        .when(id == "close", |d| {
            d.hover(|d| d.bg(gpui_kit::Hsla::from(gpui_kit::rgb(0xc42b1c))))
        })
        // Keep the title bar's drag handler from grabbing the pointer.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| action(window, cx))
        .child(glyph)
}

fn resize_edge(pos: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let (left, right) = (pos.x < INSET, pos.x > size.width - INSET);
    let (top, bottom) = (pos.y < INSET, pos.y > size.height - INSET);
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (true, _, _, true) => ResizeEdge::TopRight,
        (_, true, true, _) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (true, ..) => ResizeEdge::Top,
        (_, true, ..) => ResizeEdge::Bottom,
        (_, _, true, _) => ResizeEdge::Left,
        (_, _, _, true) => ResizeEdge::Right,
        _ => return None,
    })
}

const fn cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}
