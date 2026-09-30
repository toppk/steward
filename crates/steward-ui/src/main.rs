//! `steward-ui`: a qdirstat-style directory tree over the steward index.
//! Everything shown comes from `stewardd`; this process never walks the disk.

mod client;
mod content;
mod decorations;
mod format;
mod settings;
mod shell;
mod tree;

use std::path::PathBuf;

use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, point, px, size};

fn main() {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let root = std::path::absolute(&root).unwrap_or(root);

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_omarchy::init(cx);
            shell::bind_keys(cx);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(100.), px(80.)),
                            size(px(1280.), px(820.)),
                        ))),
                        titlebar: Some(gpui_kit::TitlebarOptions {
                            title: Some("steward".into()),
                            ..Default::default()
                        }),
                        app_id: Some("steward-ui".to_owned()),
                        // Transparent so the resize margin around the frame is see-through.
                        window_background: gpui_kit::WindowBackgroundAppearance::Transparent,
                        window_decorations: Some(gpui_kit::WindowDecorations::Server),
                        window_min_size: Some(size(px(640.), px(400.))),
                        ..Default::default()
                    },
                    |window, cx| cx.new(|cx| shell::Shell::new(root.clone(), window, cx)),
                )
                .expect("open the steward window");
            let _ = window.update(cx, |this, window, cx| this.focus_active(window, cx));
            cx.activate(true);
        });
}
