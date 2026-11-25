//! The main window: toolbar, filters, virtualised log list, detail pane and status bar.

mod control;

use crate::adb::{self, Device, Msg};
use crate::input::{InputEvent, TextInput};
use crate::mcp::{Reply, UiRequest};
use crate::log::{self, Filter, Level, LogEntry, Parser};
use crate::theme::{Palette, Settings, ThemeMode};
use gpui::{prelude::*, *};
use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

actions!(
    log_viewer,
    [FocusSearch, OpenFile, SaveFile, ClearLog, CopyLine, SelectAll, ToggleFollow, TogglePause, Quit]
);

#[cfg(target_os = "windows")]
const MONO: &str = "Consolas";
#[cfg(target_os = "macos")]
const MONO: &str = "Menlo";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const MONO: &str = "DejaVu Sans Mono";

const ROW_H: f32 = 20.0;
/// Oldest lines are dropped once a live stream grows past this.
const MAX_LIVE_ENTRIES: usize = 400_000;
const MAX_DISPLAY_CHARS: usize = 1500;

const W_TIME: f32 = 138.0;
const W_PID: f32 = 52.0;
const W_LEVEL: f32 = 16.0;
const W_TAG: f32 = 170.0;

enum Source {
    None,
    Device(String),
    File(PathBuf),
}

pub struct LogViewer {
    settings: Settings,
    palette: Palette,

    entries: Vec<LogEntry>,
    /// Indices into `entries` that pass the current filter.
    visible: Vec<usize>,
    counts: [usize; 6],
    filter: Filter,
    /// Cursor line (entry index) used for keyboard navigation.
    selected: Option<usize>,
    /// All selected lines (entry indices).
    selection: BTreeSet<usize>,
    anchor: Option<usize>,
    select_drag: bool,
    follow: bool,
    paused: bool,
    source: Source,
    status: SharedString,
    status_error: bool,
    title: String,

    adb: PathBuf,
    devices: Vec<Device>,
    device: Option<String>,
    devices_open: bool,
    scanning: bool,
    stream: Option<Child>,
    session: u64,
    streaming: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    parser: Parser,
    mcp_rx: Option<Receiver<UiRequest>>,
    scrollbar_drag: bool,

    search: Entity<TextInput>,
    address: Entity<TextInput>,
    list_scroll: UniformListScrollHandle,
    list_focus: FocusHandle,
    root_focus: FocusHandle,
    _pump: Task<()>,
    _subs: Vec<Subscription>,
}

impl LogViewer {
    pub fn new(window: &mut Window, mcp_rx: Option<Receiver<UiRequest>>, cx: &mut Context<Self>) -> Self {
        let settings = Settings::load();
        let palette = settings.theme.palette(window.appearance());
        let (tx, rx) = channel();

        let search = cx.new(|cx| TextInput::new("Filter log (Ctrl+F)", palette, cx));
        let address = cx.new(|cx| {
            let mut input = TextInput::new("host:port, e.g. 192.168.1.20:5555", palette, cx);
            input.set_text(&settings.last_address, cx);
            input
        });

        let mut subs = Vec::new();
        subs.push(cx.subscribe(&search, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Changed = event {
                let text = input.read(cx).text().to_string();
                this.set_query(text, cx);
            }
        }));
        subs.push(cx.subscribe(&address, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Submit = event {
                this.connect_address(cx);
            }
        }));
        subs.push(cx.observe_window_appearance(window, |_, _, cx| cx.notify()));

        let pump = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(50)).await;
            if this.update(cx, |this, cx| this.pump(cx)).is_err() {
                break;
            }
        });

        let adb = adb::find_adb(settings.adb_path.as_deref());
        let mut this = LogViewer {
            settings,
            palette,
            entries: Vec::new(),
            visible: Vec::new(),
            counts: [0; 6],
            filter: Filter::default(),
            selected: None,
            selection: BTreeSet::new(),
            anchor: None,
            select_drag: false,
            follow: true,
            paused: false,
            source: Source::None,
            status: "Open a log file or pick a device to begin.".into(),
            status_error: false,
            title: String::new(),
            adb,
            devices: Vec::new(),
            device: None,
            devices_open: false,
            scanning: false,
            stream: None,
            session: 0,
            streaming: false,
            tx,
            rx,
            parser: Parser::default(),
            mcp_rx,
            scrollbar_drag: false,
            search,
            address,
            list_scroll: UniformListScrollHandle::new(),
            list_focus: cx.focus_handle(),
            root_focus: cx.focus_handle(),
            _pump: pump,
            _subs: subs,
        };
        this.refresh_devices(cx);
        window.focus(&this.root_focus);
        this
    }

    /// Open a file given on the command line.
    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_file(path, cx);
    }

    // ---- status helpers -------------------------------------------------------------------

    fn info(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = msg.into();
        self.status_error = false;
        cx.notify();
    }

    fn error(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = msg.into();
        self.status_error = true;
        cx.notify();
    }

    // ---- data -----------------------------------------------------------------------------

    fn push(&mut self, entry: LogEntry) {
        self.counts[entry.level as usize] += 1;
        if self.filter.matches(&entry) {
            self.visible.push(self.entries.len());
        }
        self.entries.push(entry);
    }

    fn rebuild_visible(&mut self) {
        self.visible.clear();
        self.visible.extend(
            self.entries.iter().enumerate().filter(|(_, e)| self.filter.matches(e)).map(|(i, _)| i),
        );
        // Lines hidden by the filter are no longer selected (so a copy never includes them).
        let visible = &self.visible;
        self.selection.retain(|e| visible.binary_search(e).is_ok());
    }

    fn recount(&mut self) {
        self.counts = [0; 6];
        for e in &self.entries {
            self.counts[e.level as usize] += 1;
        }
    }

    fn enforce_cap(&mut self) {
        if self.entries.len() <= MAX_LIVE_ENTRIES {
            return;
        }
        let drop = self.entries.len() - MAX_LIVE_ENTRIES * 9 / 10;
        self.entries.drain(..drop);
        self.selected = self.selected.and_then(|s| s.checked_sub(drop));
        self.anchor = self.anchor.and_then(|s| s.checked_sub(drop));
        self.selection = self.selection.iter().filter_map(|s| s.checked_sub(drop)).collect();
        self.recount();
        self.rebuild_visible();
    }

    fn clear_entries(&mut self) {
        self.entries.clear();
        self.visible.clear();
        self.counts = [0; 6];
        self.selected = None;
        self.anchor = None;
        self.selection.clear();
        self.parser = Parser::default();
    }

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.filter.query = query;
        self.refilter(cx);
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        self.filter.recompile();
        let invalid = self.filter.error.is_some();
        self.search.update(cx, |s, _| s.invalid = invalid);
        self.rebuild_visible();
        if self.follow {
            self.scroll_to_end();
        }
        cx.notify();
    }

    fn scroll_to_end(&self) {
        if !self.visible.is_empty() {
            self.list_scroll.scroll_to_item(self.visible.len() - 1, ScrollStrategy::Bottom);
        }
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        self.drain_mcp(cx);
        if self.paused {
            return;
        }
        let mut changed = false;
        for _ in 0..50_000 {
            match self.rx.try_recv() {
                Ok(Msg::Line(session, line)) if session == self.session => {
                    let entry = self.parser.parse(&line);
                    self.push(entry);
                    changed = true;
                }
                Ok(Msg::Ended(session, err)) if session == self.session => {
                    self.stop_stream();
                    match err {
                        Some(e) => self.error(format!("adb: {e}"), cx),
                        None => self.error("Stream ended (device disconnected?)", cx),
                    }
                    changed = true;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if changed {
            self.enforce_cap();
            if self.follow {
                self.scroll_to_end();
            }
            cx.notify();
        }
    }

    // ---- adb ------------------------------------------------------------------------------

    fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        self.refresh_devices_then(cx, None);
    }

    /// Scans for devices; `then` runs once the scan has finished (also when one is already running).
    fn refresh_devices_then(&mut self, cx: &mut Context<Self>, then: Option<Box<dyn FnOnce(&mut Self, &mut Context<Self>)>>) {
        if self.scanning && then.is_none() {
            return;
        }
        self.scanning = true;
        let adb = self.adb.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { adb::list_devices(&adb) }).await;
            this.update(cx, |this, cx| {
                this.scanning = false;
                match result {
                    Ok(devices) => {
                        let ready: Vec<_> = devices.iter().filter(|d| d.is_ready()).collect();
                        let keep = this.device.as_ref().is_some_and(|s| ready.iter().any(|d| &d.serial == s));
                        if !keep {
                            this.device = (ready.len() == 1).then(|| ready[0].serial.clone());
                        }
                        let n = ready.len();
                        this.devices = devices;
                        if this.streaming || !matches!(this.source, Source::None) {
                            cx.notify();
                        } else if n == 0 {
                            this.error("No Android device found - plug one in with USB debugging enabled.", cx);
                        } else {
                            this.info(format!("{n} device(s) available."), cx);
                        }
                    }
                    Err(e) => this.error(format!("{e:#}"), cx),
                }
                if let Some(then) = then {
                    then(this, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn connect_address(&mut self, cx: &mut Context<Self>) {
        let addr = self.address.read(cx).text().trim().to_string();
        if addr.is_empty() {
            return;
        }
        self.connect_to(addr, cx, None);
    }

    fn connect_to(&mut self, addr: String, cx: &mut Context<Self>, done: Option<Reply>) {
        self.settings.last_address = addr.clone();
        self.settings.save();
        self.info(format!("Connecting to {addr}..."), cx);
        let adb = self.adb.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { adb::connect(&adb, &addr) }).await;
            this.update(cx, |this, cx| {
                let outcome = match result {
                    Ok(msg) => {
                        this.info(msg.clone(), cx);
                        Ok(msg)
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        this.error(e.clone(), cx);
                        Err(e)
                    }
                };
                this.refresh_devices_then(
                    cx,
                    Some(Box::new(move |this, _| {
                        if let Some(done) = done {
                            let _ = done.send(outcome.map(|m| this.devices_json(m)));
                        }
                    })),
                );
            })
            .ok();
        })
        .detach();
    }

    fn start_stream(&mut self, cx: &mut Context<Self>) {
        let Some(serial) = self.device.clone() else {
            self.devices_open = true;
            self.error("Select a device first.", cx);
            return;
        };
        self.stop_stream();
        self.clear_entries();
        self.session += 1;
        match adb::start_logcat(&self.adb, &serial, self.session, self.tx.clone()) {
            Ok(child) => {
                self.stream = Some(child);
                self.streaming = true;
                self.follow = true;
                let label = self
                    .devices
                    .iter()
                    .find(|d| d.serial == serial)
                    .map_or(serial.clone(), Device::label);
                self.info(format!("Streaming logcat from {label}"), cx);
                self.source = Source::Device(label);
            }
            Err(e) => self.error(format!("{e:#}"), cx),
        }
    }

    fn stop_stream(&mut self) {
        if let Some(mut child) = self.stream.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.streaming = false;
        self.paused = false;
    }

    // ---- files ----------------------------------------------------------------------------

    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open log file".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |this, cx| this.load_file(path, cx)).ok();
                }
            }
        })
        .detach();
    }

    fn load_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_file_then(path, cx, None);
    }

    fn load_file_then(&mut self, path: PathBuf, cx: &mut Context<Self>, done: Option<Reply>) {
        self.stop_stream();
        self.info(format!("Loading {}...", path.display()), cx);
        let p = path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let bytes = std::fs::read(&p)?;
                    Ok::<_, std::io::Error>(log::parse_text(&String::from_utf8_lossy(&bytes)))
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(entries) => {
                    this.clear_entries();
                    this.counts = [0; 6];
                    for e in entries {
                        this.push(e);
                    }
                    this.follow = false;
                    this.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    let n = this.entries.len();
                    this.source = Source::File(path.clone());
                    this.info(format!("Loaded {n} lines from {}", path.display()), cx);
                    if let Some(done) = done {
                        let _ = done.send(Ok(serde_json::json!({ "path": path, "lines": n, "visible": this.visible.len() })));
                    }
                }
                Err(e) => {
                    let msg = format!("Could not read {}: {e}", path.display());
                    this.error(msg.clone(), cx);
                    if let Some(done) = done {
                        let _ = done.send(Err(msg));
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn save_dialog(&mut self, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            self.error("Nothing to save.", cx);
            return;
        }
        let dir = match &self.source {
            Source::File(p) => p.parent().map(PathBuf::from),
            _ => None,
        }
        .or_else(dirs::document_dir)
        .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some("logcat.log"));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            this.update(cx, |this, cx| this.save_to(path, false, cx, None)).ok();
        })
        .detach();
    }

    /// Writes the visible (or, with `all`, every) line to `path`.
    fn save_to(&mut self, path: PathBuf, all: bool, cx: &mut Context<Self>, done: Option<Reply>) {
        let mut text = String::new();
        let lines = if all { self.entries.len() } else { self.visible.len() };
        if all {
            for e in &self.entries {
                text.push_str(e.raw());
                text.push('\n');
            }
        } else {
            for &i in &self.visible {
                text.push_str(self.entries[i].raw());
                text.push('\n');
            }
        }
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let result = cx.background_executor().spawn(async move { std::fs::write(&p, text) }).await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.info(format!("Saved {lines} lines to {}", path.display()), cx);
                    if let Some(done) = done {
                        let _ = done.send(Ok(serde_json::json!({ "path": path, "lines": lines })));
                    }
                }
                Err(e) => {
                    let msg = format!("Could not save {}: {e}", path.display());
                    this.error(msg.clone(), cx);
                    if let Some(done) = done {
                        let _ = done.send(Err(msg));
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Copies every selected line (in log order) to the clipboard.
    fn copy_selected(&mut self, cx: &mut Context<Self>) {
        let lines: Vec<&str> = self.selection.iter().filter_map(|&i| self.entries.get(i)).map(LogEntry::raw).collect();
        if lines.is_empty() {
            return;
        }
        let n = lines.len();
        let sep = if cfg!(windows) { "\r\n" } else { "\n" };
        cx.write_to_clipboard(ClipboardItem::new_string(lines.join(sep)));
        self.info(format!("Copied {n} line{} to clipboard.", if n == 1 { "" } else { "s" }), cx);
    }

    fn select_only(&mut self, entry: usize) {
        self.selection.clear();
        self.selection.insert(entry);
        self.selected = Some(entry);
        self.anchor = Some(entry);
    }

    fn toggle_entry(&mut self, entry: usize, cx: &mut Context<Self>) {
        if !self.selection.remove(&entry) {
            self.selection.insert(entry);
        }
        self.selected = Some(entry);
        self.anchor = Some(entry);
        cx.notify();
    }

    /// Selects everything between the anchor and visible row `vi`.
    fn select_range_to(&mut self, vi: usize, cx: &mut Context<Self>) {
        let Some(&target) = self.visible.get(vi) else { return };
        let anchor_vi = self.anchor.and_then(|a| self.visible.binary_search(&a).ok());
        if anchor_vi.is_none() {
            self.anchor = Some(target);
        }
        let from = anchor_vi.unwrap_or(vi);
        self.selection = self.visible[from.min(vi)..=from.max(vi)].iter().copied().collect();
        self.selected = Some(target);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection = self.visible.iter().copied().collect();
        self.selected = self.visible.last().copied();
        self.anchor = self.visible.first().copied();
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.clear_entries();
        if !self.streaming {
            self.source = Source::None;
        }
        self.info("Cleared.", cx);
    }

    fn set_theme(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        self.settings.theme = mode;
        self.settings.save();
        cx.notify();
    }

    // ---- keyboard navigation in the list --------------------------------------------------

    fn move_selection(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let current = self.selected.and_then(|s| self.visible.binary_search(&s).ok());
        let last = self.visible.len() - 1;
        let next = match current {
            Some(c) => (c as isize + delta).clamp(0, last as isize) as usize,
            None if delta < 0 => last,
            None => 0,
        };
        self.select_to(next, extend, cx);
    }

    /// Moves the cursor to visible row `vi`, either replacing the selection or extending it (Shift).
    fn select_to(&mut self, vi: usize, extend: bool, cx: &mut Context<Self>) {
        if extend {
            self.select_range_to(vi, cx);
            self.scroll_into_view(vi);
        } else {
            self.select_visible(vi, cx);
        }
    }

    fn select_visible(&mut self, vi: usize, cx: &mut Context<Self>) {
        if let Some(&entry) = self.visible.get(vi) {
            self.select_only(entry);
        }
        self.scroll_into_view(vi);
        cx.notify();
    }

    fn scroll_into_view(&mut self, vi: usize) {
        let state = self.list_scroll.0.borrow();
        let offset = -f32::from(state.base_handle.offset().y);
        let height = f32::from(state.base_handle.bounds().size.height);
        drop(state);
        let first = (offset / ROW_H).ceil().max(0.0) as usize;
        let rows = ((height / ROW_H).floor() as usize).max(1);
        if vi < first {
            self.list_scroll.scroll_to_item(vi, ScrollStrategy::Top);
        } else if vi + 1 > first + rows {
            self.list_scroll.scroll_to_item(vi, ScrollStrategy::Bottom);
        }
    }

    fn page_rows(&self) -> isize {
        let h = f32::from(self.list_scroll.0.borrow().base_handle.bounds().size.height);
        ((h / ROW_H) as isize - 1).max(1)
    }

    /// (content height, viewport height, scrolled distance) in pixels.
    fn scroll_metrics(&self) -> (f32, f32, f32) {
        let state = self.list_scroll.0.borrow();
        let viewport = f32::from(state.base_handle.bounds().size.height);
        let offset = -f32::from(state.base_handle.offset().y);
        (self.visible.len() as f32 * ROW_H, viewport, offset.max(0.0))
    }

    fn thumb_height(total: f32, viewport: f32) -> f32 {
        (viewport * viewport / total).clamp(28.0_f32.min(viewport), viewport)
    }

    /// Scroll to an absolute pixel offset from the top, clamped to the content.
    fn scroll_to_offset(&mut self, offset: f32, cx: &mut Context<Self>) {
        let (total, viewport, _) = self.scroll_metrics();
        let max = (total - viewport).max(0.0);
        let offset = offset.clamp(0.0, max);
        {
            let state = self.list_scroll.0.borrow();
            let x = state.base_handle.offset().x;
            state.base_handle.set_offset(point(x, px(-offset)));
        }
        self.follow = max - offset < 1.0 && self.streaming;
        cx.notify();
    }

    fn scroll_by_rows(&mut self, rows: isize, cx: &mut Context<Self>) {
        let (_, _, offset) = self.scroll_metrics();
        self.scroll_to_offset(offset + rows as f32 * ROW_H, cx);
    }

    /// Moves the scrollbar thumb so it is centred on window-space `y`.
    fn drag_scrollbar_to(&mut self, y: f32, cx: &mut Context<Self>) {
        let (total, viewport, _) = self.scroll_metrics();
        if total <= viewport {
            return;
        }
        let top = f32::from(self.list_scroll.0.borrow().base_handle.bounds().origin.y);
        let thumb = Self::thumb_height(total, viewport);
        let fraction = ((y - top - thumb / 2.0) / (viewport - thumb)).clamp(0.0, 1.0);
        self.scroll_to_offset(fraction * (total - viewport), cx);
    }

    /// Keyboard scrolling that works whichever widget has focus (text inputs swallow their own keys).
    fn on_nav_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform {
            return;
        }
        let page = self.page_rows();
        let last = self.visible.len().saturating_sub(1);
        let has_selection = self.selected.is_some();
        let extend = m.shift;
        match ev.keystroke.key.as_str() {
            "up" if has_selection => self.move_selection(-1, extend, cx),
            "down" if has_selection => self.move_selection(1, extend, cx),
            "pageup" if has_selection => self.move_selection(-page, extend, cx),
            "pagedown" if has_selection => self.move_selection(page, extend, cx),
            "home" if has_selection => self.select_to(0, extend, cx),
            "end" if has_selection => self.select_to(last, extend, cx),
            "up" => self.scroll_by_rows(-3, cx),
            "down" => self.scroll_by_rows(3, cx),
            "pageup" => self.scroll_by_rows(-page, cx),
            "pagedown" => self.scroll_by_rows(page, cx),
            "home" => self.scroll_to_offset(0.0, cx),
            "end" => self.scroll_to_offset(f32::MAX, cx),
            _ => return,
        }
        if has_selection {
            self.follow = false;
        }
        cx.stop_propagation();
    }
}

impl Drop for LogViewer {
    fn drop(&mut self) {
        self.stop_stream();
    }
}

fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, p: &Palette, active: bool, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.))
        .px_3()
        .flex()
        .flex_none()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(if active { p.accent } else { p.border })
        .bg(if active { p.accent } else { p.panel })
        .text_color(if active { p.accent_text } else { p.text })
        .when(enabled, |b| {
            b.cursor_pointer().hover(|s| s.bg(if active { p.accent } else { p.button_hover }))
        })
        .when(!enabled, |b| b.opacity(0.4))
        .child(label.into())
}

fn truncated(s: &str) -> String {
    match s.char_indices().nth(MAX_DISPLAY_CHARS) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}

impl Render for LogViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.settings.theme.palette(window.appearance());
        self.palette = p;
        self.search.update(cx, |s, _| s.palette = p);
        self.address.update(cx, |s, _| s.palette = p);

        let title = match &self.source {
            Source::None => "Log Viewer".to_string(),
            Source::Device(d) => format!("Log Viewer - {d}"),
            Source::File(f) => format!("Log Viewer - {}", f.display()),
        };
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }

        div()
            .id("root")
            .key_context("LogViewer")
            .track_focus(&self.root_focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .text_color(p.text)
            .text_size(px(13.))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_nav_key(ev, cx)))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.scrollbar_drag {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        this.drag_scrollbar_to(f32::from(ev.position.y), cx);
                    } else {
                        this.scrollbar_drag = false;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.select_drag = false;
                    if this.scrollbar_drag {
                        this.scrollbar_drag = false;
                        cx.notify();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                window.focus(&this.search.read(cx).focus.clone());
            }))
            .on_action(cx.listener(|this, _: &OpenFile, _, cx| this.open_dialog(cx)))
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| this.save_dialog(cx)))
            .on_action(cx.listener(|this, _: &ClearLog, _, cx| this.clear(cx)))
            .on_action(cx.listener(|this, _: &CopyLine, _, cx| this.copy_selected(cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &ToggleFollow, _, cx| {
                this.follow = !this.follow;
                if this.follow {
                    this.scroll_to_end();
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TogglePause, _, cx| {
                if this.streaming {
                    this.paused = !this.paused;
                    cx.notify();
                }
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(path) = paths.paths().first() {
                    this.load_file(path.clone(), cx);
                }
            }))
            .child(self.render_toolbar(&p, cx))
            .child(self.render_filters(&p, cx))
            .child(self.render_header(&p))
            .child(self.render_list(&p, cx))
            .child(self.render_status(&p, cx))
    }
}

impl LogViewer {
    fn render_toolbar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let device_label = match self.device.as_ref().and_then(|s| self.devices.iter().find(|d| &d.serial == s)) {
            Some(d) => d.label(),
            None => self.device.clone().unwrap_or_else(|| "No device".into()),
        };
        let has_device = self.device.is_some();
        let streaming = self.streaming;
        let theme = self.settings.theme;

        let mut theme_buttons = div().flex().gap_1().flex_none();
        for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            theme_buttons = theme_buttons.child(
                button(("theme", mode as usize), mode.label(), p, theme == mode, true)
                    .px_2()
                    .on_click(cx.listener(move |this, _, _, cx| this.set_theme(mode, cx))),
            );
        }

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .p_2()
            .bg(p.panel)
            .border_b_1()
            .border_color(p.border)
            .child(
                div()
                    .relative()
                    .child(
                        button("device", format!("{device_label}  v"), p, self.devices_open, true)
                            .max_w(px(320.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.devices_open = !this.devices_open;
                                if this.devices_open {
                                    this.refresh_devices(cx);
                                }
                                cx.notify();
                            })),
                    )
                    .when(self.devices_open, |d| d.child(self.render_device_popover(p, cx))),
            )
            .child(if streaming {
                button("stop", "Stop", p, true, true)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop_stream();
                        this.info("Stopped.", cx);
                    }))
            } else {
                button("start", "Start", p, false, has_device)
                    .when(has_device, |b| b.on_click(cx.listener(|this, _, _, cx| this.start_stream(cx))))
            })
            .child(
                button("pause", if self.paused { "Resume" } else { "Pause" }, p, self.paused, streaming)
                    .when(streaming, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| {
                            this.paused = !this.paused;
                            cx.notify();
                        }))
                    }),
            )
            .child(button("clear", "Clear", p, false, true).on_click(cx.listener(|this, _, _, cx| this.clear(cx))))
            .child(div().w_px().h(px(20.)).bg(p.border))
            .child(button("open", "Open...", p, false, true).on_click(cx.listener(|this, _, _, cx| this.open_dialog(cx))))
            .child(button("save", "Save...", p, false, true).on_click(cx.listener(|this, _, _, cx| this.save_dialog(cx))))
            .child(div().flex_1())
            .child(div().text_color(p.text_dim).child("Theme"))
            .child(theme_buttons)
    }

    fn render_device_popover(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_1();
        if self.devices.is_empty() {
            list = list.child(
                div().p_2().text_color(p.text_dim).child(if self.scanning {
                    "Scanning..."
                } else {
                    "No devices. Plug one in with USB debugging enabled, or connect over the network below."
                }),
            );
        }
        for (i, d) in self.devices.iter().enumerate() {
            let serial = d.serial.clone();
            let ready = d.is_ready();
            let selected = self.device.as_deref() == Some(d.serial.as_str());
            list = list.child(
                div()
                    .id(("dev", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(30.))
                    .rounded_md()
                    .bg(if selected { p.selected } else { p.panel })
                    .when(ready, |r| r.cursor_pointer().hover(|s| s.bg(p.row_hover)))
                    .when(!ready, |r| r.opacity(0.5))
                    .child(div().flex_1().truncate().child(d.label()))
                    .child(div().text_color(if ready { p.text_dim } else { p.error_text }).child(d.state.clone()))
                    .when(ready, |r| {
                        r.on_click(cx.listener(move |this, _, _, cx| {
                            this.device = Some(serial.clone());
                            this.devices_open = false;
                            cx.notify();
                        }))
                    }),
            );
        }

        deferred(
            div()
                .absolute()
                .top(px(32.))
                .left_0()
                .w(px(400.))
                .occlude()
                .flex()
                .flex_col()
                .gap_2()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(p.border)
                .bg(p.panel)
                .shadow_lg()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().font_weight(FontWeight::BOLD).child("Devices"))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    button("refresh", if self.scanning { "Scanning..." } else { "Refresh" }, p, false, !self.scanning)
                                        .on_click(cx.listener(|this, _, _, cx| this.refresh_devices(cx))),
                                )
                                .child(button("close-devices", "Close", p, false, true).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.devices_open = false;
                                        cx.notify();
                                    }),
                                )),
                        ),
                )
                .child(list)
                .child(div().h_px().bg(p.border))
                .child(div().text_color(p.text_dim).child("Connect over network (adb connect)"))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(div().flex_1().child(self.address.clone()))
                        .child(button("connect", "Connect", p, false, true).on_click(
                            cx.listener(|this, _, _, cx| this.connect_address(cx)),
                        )),
                ),
        )
        .with_priority(1)
    }

    fn render_filters(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut chips = div().flex().flex_none().gap_1();
        for level in Level::ALL {
            let on = self.filter.levels[level as usize];
            let color = p.level(level);
            chips = chips.child(
                div()
                    .id(("level", level as usize))
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .cursor_pointer()
                    .border_color(if on { color } else { p.border })
                    .bg(if on { color.opacity(0.16) } else { p.panel })
                    .text_color(if on { color } else { p.text_dim })
                    .hover(|s| s.bg(p.button_hover))
                    .child(level.name())
                    .child(div().text_color(p.text_dim).text_xs().child(self.counts[level as usize].to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.filter.levels[level as usize] ^= true;
                        this.refilter(cx);
                    })),
            );
        }

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(p.border)
            .child(div().flex_1().min_w_0().child(self.search.clone()))
            .child(
                button("case", "Aa", p, self.filter.case_sensitive, true)
                    .px_2()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.filter.case_sensitive ^= true;
                        this.refilter(cx);
                    })),
            )
            .child(button("regex", ".*", p, self.filter.regex, true).px_2().on_click(cx.listener(|this, _, _, cx| {
                this.filter.regex ^= true;
                this.refilter(cx);
            })))
            .child(div().w_px().h(px(20.)).bg(p.border))
            .child(chips)
    }

    fn render_header(&self, p: &Palette) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(22.))
            .bg(p.panel)
            .border_b_1()
            .border_color(p.border)
            .text_xs()
            .text_color(p.text_dim)
            .child(div().w(px(W_TIME)).flex_none().child("Time"))
            .child(div().w(px(W_PID)).flex_none().child("PID"))
            .child(div().w(px(W_PID)).flex_none().child("TID"))
            .child(div().w(px(W_LEVEL)).flex_none().child("L"))
            .child(div().w(px(W_TAG)).flex_none().child("Tag"))
            .child(div().flex_1().child("Message"))
    }

    fn render_list(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let list = uniform_list(
            "log-list",
            self.visible.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let p = this.palette;
                range
                    .filter_map(|vi| {
                        let ei = *this.visible.get(vi)?;
                        let e = this.entries.get(ei)?;
                        let level = e.level;
                        let selected = this.selection.contains(&ei);
                        let color = p.level(level);
                        let bg = if selected {
                            p.selected
                        } else if matches!(level, Level::Warn | Level::Error | Level::Fatal) {
                            p.level_bg(level)
                        } else if vi % 2 == 1 {
                            p.row_alt
                        } else {
                            gpui::transparent_black()
                        };
                        let has_fields = !e.time().is_empty() || !e.tag().is_empty();
                        Some(
                            div()
                                .id(("row", vi))
                                .w_full()
                                .h(px(ROW_H))
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .bg(bg)
                                .text_color(color)
                                .hover(|s| s.bg(p.row_hover))
                                .when(has_fields, |r| {
                                    r.child(div().w(px(W_TIME)).flex_none().text_color(p.text_dim).child(e.time().to_string()))
                                        .child(div().w(px(W_PID)).flex_none().text_color(p.text_dim).child(e.pid().to_string()))
                                        .child(div().w(px(W_PID)).flex_none().text_color(p.text_dim).child(e.tid().to_string()))
                                        .child(div().w(px(W_LEVEL)).flex_none().font_weight(FontWeight::BOLD).child(level.letter()))
                                        .child(div().w(px(W_TAG)).flex_none().truncate().child(e.tag().to_string()))
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(truncated(if has_fields { e.message() } else { e.raw() })),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                        window.focus(&this.list_focus);
                                        if ev.modifiers.shift {
                                            this.select_range_to(vi, cx);
                                        } else if ev.modifiers.secondary() {
                                            this.toggle_entry(ei, cx);
                                        } else {
                                            this.select_only(ei);
                                            this.select_drag = true;
                                            cx.notify();
                                        }
                                    }),
                                )
                                .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                                    if this.select_drag && ev.pressed_button == Some(MouseButton::Left) {
                                        this.select_range_to(vi, cx);
                                    }
                                }))
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                        window.focus(&this.list_focus);
                                        if !this.selection.contains(&ei) {
                                            this.select_only(ei);
                                        }
                                        this.copy_selected(cx);
                                    }),
                                ),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.list_scroll.clone())
        .size_full();

        let empty = self.visible.is_empty();
        let hint = if self.entries.is_empty() {
            "No log lines yet. Start a device stream, open a file, or drop one here."
        } else {
            "No lines match the current filter."
        };

        div()
            .id("list-container")
            .key_context("LogList")
            .track_focus(&self.list_focus)
            .flex_1()
            .min_h_0()
            .relative()
            .font_family(MONO)
            .text_size(px(12.5))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                if this.follow && ev.delta.pixel_delta(px(ROW_H)).y > px(0.) {
                    this.follow = false;
                    cx.notify();
                }
            }))
            .child(list)
            .children(self.render_scrollbar(p, cx))
            .when(empty, |d| {
                d.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(p.text_dim)
                        .child(hint),
                )
            })
    }

    fn render_scrollbar(&mut self, p: &Palette, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (total, viewport, offset) = self.scroll_metrics();
        if viewport <= 0.0 || total <= viewport {
            return None;
        }
        let thumb_h = Self::thumb_height(total, viewport);
        let top = offset / (total - viewport) * (viewport - thumb_h);
        let dragging = self.scrollbar_drag;
        Some(
            div()
                .id("scrollbar")
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(16.))
                .bg(p.panel.opacity(0.6))
                .border_l_1()
                .border_color(p.border)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        this.scrollbar_drag = true;
                        this.drag_scrollbar_to(f32::from(ev.position.y), cx);
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(top))
                        .left(px(3.))
                        .w(px(10.))
                        .h(px(thumb_h))
                        .rounded_full()
                        .bg(if dragging { p.accent } else { p.text_dim.opacity(0.65) }),
                ),
        )
    }

    fn render_status(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = self.visible.len();
        let total = self.entries.len();
        let counts = if self.filter.is_active() {
            format!("{shown} of {total} lines")
        } else {
            format!("{total} lines")
        };
        let regex_error = self.filter.error.clone();
        let live = if self.paused {
            Some(("PAUSED", p.level(Level::Warn)))
        } else if self.streaming {
            Some(("LIVE", p.level(Level::Info)))
        } else {
            None
        };

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_3()
            .px_2()
            .h(px(26.))
            .bg(p.panel)
            .border_t_1()
            .border_color(p.border)
            .text_xs()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(if self.status_error || regex_error.is_some() { p.error_text } else { p.text_dim })
                    .child(match regex_error {
                        Some(e) => SharedString::from(format!("Invalid regex: {e}")),
                        None => self.status.clone(),
                    }),
            )
            .children(live.map(|(t, c)| div().text_color(c).font_weight(FontWeight::BOLD).child(t)))
            .child(div().text_color(p.text_dim).child(counts))
            .child(
                div()
                    .id("follow")
                    .cursor_pointer()
                    .px_2()
                    .rounded_sm()
                    .text_color(if self.follow { p.accent_text } else { p.text_dim })
                    .bg(if self.follow { p.accent } else { gpui::transparent_black() })
                    .child("Follow")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.follow = !this.follow;
                        if this.follow {
                            this.scroll_to_end();
                        }
                        cx.notify();
                    })),
            )
    }
}
