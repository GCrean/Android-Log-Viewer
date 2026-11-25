//! A minimal single-line text input (no selection or IME; supports caret movement,
//! editing and clipboard paste/copy of the whole value).

use crate::theme::Palette;
use gpui::*;

pub enum InputEvent {
    Changed,
    Submit,
}

pub struct TextInput {
    text: String,
    /// Caret position as a byte offset.
    cursor: usize,
    placeholder: SharedString,
    pub focus: FocusHandle,
    pub palette: Palette,
    pub invalid: bool,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(placeholder: &str, palette: Palette, cx: &mut Context<Self>) -> Self {
        TextInput {
            text: String::new(),
            cursor: 0,
            placeholder: placeholder.to_string().into(),
            focus: cx.focus_handle(),
            palette,
            invalid: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        cx.notify();
    }

    fn prev_boundary(&self, from: usize) -> usize {
        self.text[..from].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.text[from..].chars().next().map_or(from, |c| from + c.len_utf8())
    }

    fn word_start(&self, from: usize) -> usize {
        let head = self.text[..from].trim_end();
        head.rfind(char::is_whitespace).map_or(0, |i| i + 1)
    }

    fn insert(&mut self, s: &str, cx: &mut Context<Self>) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn delete_range(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        if start < end {
            self.text.replace_range(start..end, "");
            self.cursor = start;
            cx.emit(InputEvent::Changed);
            cx.notify();
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let m = &ks.modifiers;
        let word = m.control || m.alt;
        match ks.key.as_str() {
            "enter" => cx.emit(InputEvent::Submit),
            "escape" => {
                if !self.text.is_empty() {
                    self.text.clear();
                    self.cursor = 0;
                    cx.emit(InputEvent::Changed);
                    cx.notify();
                }
            }
            "backspace" => {
                let start = if word { self.word_start(self.cursor) } else { self.prev_boundary(self.cursor) };
                self.delete_range(start, self.cursor, cx);
            }
            "delete" => {
                let end = self.next_boundary(self.cursor);
                self.delete_range(self.cursor, end, cx);
            }
            "left" => {
                self.cursor = if word { self.word_start(self.cursor) } else { self.prev_boundary(self.cursor) };
                cx.notify();
            }
            "right" => {
                self.cursor = self.next_boundary(self.cursor);
                cx.notify();
            }
            "home" => {
                self.cursor = 0;
                cx.notify();
            }
            "end" => {
                self.cursor = self.text.len();
                cx.notify();
            }
            "v" if m.secondary() => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    let single = text.lines().next().unwrap_or("").to_string();
                    self.insert(&single, cx);
                }
            }
            "c" if m.secondary() => cx.write_to_clipboard(ClipboardItem::new_string(self.text.clone())),
            "x" if m.secondary() => {
                cx.write_to_clipboard(ClipboardItem::new_string(self.text.clone()));
                let len = self.text.len();
                self.delete_range(0, len, cx);
            }
            _ => {
                if m.control || m.platform || m.function {
                    return;
                }
                if let Some(ch) = ks.key_char.as_deref().filter(|c| !c.chars().any(char::is_control)) {
                    self.insert(ch, cx);
                }
            }
        }
        cx.stop_propagation();
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let focused = self.focus.is_focused(window);
        let border = if self.invalid {
            p.error_text
        } else if focused {
            p.accent
        } else {
            p.border
        };

        let content = if self.text.is_empty() {
            div()
                .flex()
                .items_center()
                .child(div().w(px(1.)).h(px(16.)).bg(if focused { p.text } else { gpui::transparent_black() }))
                .child(div().text_color(p.text_dim).child(self.placeholder.clone()))
        } else {
            let (before, after) = self.text.split_at(self.cursor);
            div()
                .flex()
                .items_center()
                .child(before.to_string())
                .child(div().w(px(1.)).h(px(16.)).bg(if focused { p.text } else { gpui::transparent_black() }))
                .child(after.to_string())
        };

        div()
            .id("text-input")
            .key_context("TextInput")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus);
                    this.cursor = this.text.len();
                    cx.notify();
                }),
            )
            .cursor_text()
            .w_full()
            .h(px(28.))
            .px_2()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(p.input_bg)
            .text_color(p.text)
            .child(content)
    }
}
