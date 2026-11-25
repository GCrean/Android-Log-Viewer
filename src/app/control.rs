//! Executes MCP tool calls on the GUI thread.

use super::*;
use serde_json::{json, Value};

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn bool_arg(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn uint_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

impl LogViewer {
    pub(super) fn drain_mcp(&mut self, cx: &mut Context<Self>) {
        let mut pending = Vec::new();
        if let Some(rx) = &self.mcp_rx {
            while let Ok(req) = rx.try_recv() {
                pending.push(req);
            }
        }
        for req in pending {
            self.handle_tool(req, cx);
        }
    }

    pub(super) fn devices_json(&self, message: String) -> Value {
        json!({
            "message": message,
            "selected": self.device,
            "devices": self.devices.iter().map(|d| json!({
                "serial": d.serial, "state": d.state, "model": d.description, "ready": d.is_ready(),
            })).collect::<Vec<_>>(),
        })
    }

    fn line_json(&self, visible_index: usize) -> Option<Value> {
        let e = self.entries.get(*self.visible.get(visible_index)?)?;
        Some(json!({
            "index": visible_index,
            "level": e.level.name(),
            "time": e.time(),
            "pid": e.pid(),
            "tid": e.tid(),
            "tag": e.tag(),
            "message": e.message(),
            "raw": e.raw(),
        }))
    }

    fn state_json(&self) -> Value {
        let source = match &self.source {
            Source::None => json!({ "type": "none" }),
            Source::Device(d) => json!({ "type": "device", "name": d }),
            Source::File(p) => json!({ "type": "file", "name": p }),
        };
        let counts: serde_json::Map<String, Value> =
            Level::ALL.iter().map(|l| (l.name().to_string(), json!(self.counts[*l as usize]))).collect();
        json!({
            "source": source,
            "streaming": self.streaming,
            "paused": self.paused,
            "follow": self.follow,
            "device": self.device,
            "total_lines": self.entries.len(),
            "visible_lines": self.visible.len(),
            "level_counts": counts,
            "filter": {
                "query": self.filter.query,
                "regex": self.filter.regex,
                "case_sensitive": self.filter.case_sensitive,
                "levels": Level::ALL.iter().filter(|l| self.filter.levels[**l as usize]).map(|l| l.name()).collect::<Vec<_>>(),
                "error": self.filter.error,
            },
            "selected_index": self.selected.and_then(|s| self.visible.binary_search(&s).ok()),
            "selected_count": self.selection.len(),
            "theme": self.settings.theme.label().to_lowercase(),
            "status": self.status,
            "status_is_error": self.status_error,
        })
    }

    fn handle_tool(&mut self, req: UiRequest, cx: &mut Context<Self>) {
        let UiRequest { tool, args, reply } = req;
        match tool.as_str() {
            "get_state" => {
                let _ = reply.send(Ok(self.state_json()));
            }

            "list_devices" => {
                self.refresh_devices_then(
                    cx,
                    Some(Box::new(move |this, _| {
                        let _ = reply.send(Ok(this.devices_json(String::new())));
                    })),
                );
            }

            "connect_device" => match str_arg(&args, "address").map(str::trim).filter(|a| !a.is_empty()) {
                Some(addr) => {
                    self.address.update(cx, |a, cx| a.set_text(addr, cx));
                    self.connect_to(addr.to_string(), cx, Some(reply));
                }
                None => {
                    let _ = reply.send(Err("`address` is required".into()));
                }
            },

            "start_stream" => {
                if let Some(serial) = str_arg(&args, "serial") {
                    self.device = Some(serial.to_string());
                }
                self.start_stream(cx);
                let _ = reply.send(if self.streaming { Ok(self.state_json()) } else { Err(self.status.to_string()) });
            }

            "stop_stream" => {
                self.stop_stream();
                self.info("Stopped.", cx);
                let _ = reply.send(Ok(self.state_json()));
            }

            "set_paused" => {
                let result = match bool_arg(&args, "paused") {
                    Some(p) if self.streaming => {
                        self.paused = p;
                        cx.notify();
                        Ok(self.state_json())
                    }
                    Some(_) => Err("not streaming".to_string()),
                    None => Err("`paused` (boolean) is required".to_string()),
                };
                let _ = reply.send(result);
            }

            "open_file" => match str_arg(&args, "path") {
                Some(path) => self.load_file_then(PathBuf::from(path), cx, Some(reply)),
                None => {
                    let _ = reply.send(Err("`path` is required".into()));
                }
            },

            "save_file" => match str_arg(&args, "path") {
                Some(path) => {
                    let all = bool_arg(&args, "all").unwrap_or(false);
                    self.save_to(PathBuf::from(path), all, cx, Some(reply));
                }
                None => {
                    let _ = reply.send(Err("`path` is required".into()));
                }
            },

            "clear_logs" => {
                self.clear(cx);
                let _ = reply.send(Ok(self.state_json()));
            }

            "set_filter" => {
                if let Some(q) = str_arg(&args, "query") {
                    self.filter.query = q.to_string();
                    self.search.update(cx, |s, cx| s.set_text(q, cx));
                }
                if let Some(r) = bool_arg(&args, "regex") {
                    self.filter.regex = r;
                }
                if let Some(c) = bool_arg(&args, "case_sensitive") {
                    self.filter.case_sensitive = c;
                }
                if let Some(levels) = args.get("levels").and_then(Value::as_array) {
                    let wanted: Vec<String> =
                        levels.iter().filter_map(Value::as_str).map(str::to_ascii_lowercase).collect();
                    for l in Level::ALL {
                        self.filter.levels[l as usize] = wanted.iter().any(|w| w == &l.name().to_ascii_lowercase());
                    }
                }
                self.refilter(cx);
                let _ = reply.send(match self.filter.error.clone() {
                    Some(e) => Err(format!("invalid regex: {e}")),
                    None => Ok(self.state_json()),
                });
            }

            "get_logs" => {
                let limit = uint_arg(&args, "limit").unwrap_or(100).clamp(1, 1000);
                let offset = uint_arg(&args, "offset").unwrap_or(0);
                let total = self.visible.len();
                let start = if bool_arg(&args, "from_end").unwrap_or(false) {
                    total.saturating_sub(offset + limit)
                } else {
                    offset.min(total)
                };
                let end = (start + limit).min(total);
                let lines: Vec<Value> = (start..end).filter_map(|i| self.line_json(i)).collect();
                let _ = reply.send(Ok(json!({
                    "total_visible": total, "start": start, "returned": lines.len(), "lines": lines
                })));
            }

            "select_line" => {
                let result = match uint_arg(&args, "index") {
                    Some(i) if i < self.visible.len() => {
                        self.select_visible(i, cx);
                        self.follow = false;
                        Ok(self.line_json(i).unwrap_or(Value::Null))
                    }
                    Some(_) => Err(format!("index out of range (0..{})", self.visible.len())),
                    None => Err("`index` is required".to_string()),
                };
                let _ = reply.send(result);
            }

            "set_theme" => {
                let mode = match str_arg(&args, "mode") {
                    Some("auto") => Some(ThemeMode::System),
                    Some("light") => Some(ThemeMode::Light),
                    Some("dark") => Some(ThemeMode::Dark),
                    _ => None,
                };
                let _ = reply.send(match mode {
                    Some(m) => {
                        self.set_theme(m, cx);
                        Ok(self.state_json())
                    }
                    None => Err("`mode` must be auto, light or dark".to_string()),
                });
            }

            "set_follow" => {
                let result = match bool_arg(&args, "follow") {
                    Some(f) => {
                        self.follow = f;
                        if f {
                            self.scroll_to_end();
                        }
                        cx.notify();
                        Ok(self.state_json())
                    }
                    None => Err("`follow` (boolean) is required".to_string()),
                };
                let _ = reply.send(result);
            }

            other => {
                let _ = reply.send(Err(format!("unknown tool: {other}")));
            }
        }
    }
}
