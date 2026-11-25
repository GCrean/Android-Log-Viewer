#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adb;
mod app;
mod input;
mod log;
mod mcp;
mod theme;

use app::*;
use gpui::*;
use std::cell::RefCell;
use std::path::PathBuf;

/// `log-viewer [FILE] [--mcp-stdio] [--mcp-tcp[=ADDR]]`
fn parse_args() -> (Option<PathBuf>, mcp::McpConfig) {
    let mut file = None;
    let mut config = mcp::McpConfig::default();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mcp-stdio" => config.stdio = true,
            "--mcp-tcp" => {
                let addr = args.next_if(|a| !a.starts_with("--") && a.contains(':'));
                config.tcp = Some(addr.unwrap_or_else(|| mcp::DEFAULT_TCP_ADDR.to_string()));
            }
            a if a.starts_with("--mcp-tcp=") => config.tcp = Some(a["--mcp-tcp=".len()..].to_string()),
            a if a.starts_with("--") => eprintln!("ignoring unknown option {a}"),
            _ => file = Some(PathBuf::from(arg)),
        }
    }
    (file, config)
}

fn main() {
    let (initial_file, mcp_config) = parse_args();
    let wants_mcp = mcp_config.stdio || mcp_config.tcp.is_some();
    let mcp_rx = RefCell::new(wants_mcp.then(|| mcp::start(mcp_config)));

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("secondary-f", FocusSearch, None),
            KeyBinding::new("secondary-o", OpenFile, None),
            KeyBinding::new("secondary-s", SaveFile, None),
            KeyBinding::new("secondary-l", ClearLog, None),
            KeyBinding::new("secondary-p", TogglePause, None),
            KeyBinding::new("secondary-q", Quit, None),
            KeyBinding::new("secondary-c", CopyLine, Some("LogList")),
            KeyBinding::new("secondary-a", SelectAll, Some("LogList")),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![
            Menu {
                name: "Log Viewer".into(),
                items: vec![MenuItem::action("Quit", Quit)],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("Open...", OpenFile),
                    MenuItem::action("Save...", SaveFile),
                    MenuItem::separator(),
                    MenuItem::action("Clear", ClearLog),
                ],
            },
        ]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Log Viewer".into()),
                ..Default::default()
            }),
            window_min_size: Some(size(px(760.), px(480.))),
            app_id: Some("log-viewer".into()),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            cx.new(|cx| {
                let mut view = LogViewer::new(window, mcp_rx.borrow_mut().take(), cx);
                if let Some(path) = initial_file.clone() {
                    view.open_path(path, cx);
                }
                view
            })
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
