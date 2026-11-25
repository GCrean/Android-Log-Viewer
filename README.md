# Log Viewer

A fast, GPU-accelerated log viewer written in Rust with [GPUI](https://www.gpui.rs/).
Streams `logcat` from an Android device over ADB, opens and saves log files, and filters by level and search text.

## Features

- **ADB**: auto-detects `adb` (PATH, `ANDROID_HOME`, `ANDROID_SDK_ROOT`, common SDK folders), lists USB devices,
  `adb connect host:port` for network devices, live `logcat -v threadtime` streaming with Pause / Stop / Follow.
- **Open / Save**: open any text log (logcat `threadtime` and `brief` formats are parsed into columns; other files
  are shown line-by-line with the level inferred from `ERROR`, `WARN`, ... keywords). Drag & drop a file onto the
  window, or pass it as a command-line argument. *Save* writes the lines currently visible (i.e. after filtering).
- **Filtering**: live search bar (plain text, `Aa` = match case, `.*` = regular expression) plus toggles for
  Verbose / Debug / Info / Warn / Error / Fatal, each showing its line count.
- **Themes**: Light, Dark, or Auto (follows the OS). The choice is remembered.
- Virtualised list - hundreds of thousands of lines stay smooth (live streams keep the newest 400k lines).

### Selecting and copying lines

Click a line to select it, drag (or Shift+click / Shift+Up/Down) to select a range, and Ctrl/Cmd+click to add or
remove single lines. **Right-click** copies the selected lines to the clipboard (right-clicking an unselected line
selects and copies just that one). `Ctrl/Cmd+A` selects every visible line and `Ctrl/Cmd+C` also copies.

### Shortcuts

| Key | Action |
| --- | --- |
| `Ctrl/Cmd+F` | Focus search (`Esc` clears it) |
| `Ctrl/Cmd+O` / `Ctrl/Cmd+S` | Open / Save |
| `Ctrl/Cmd+L` | Clear |
| `Ctrl/Cmd+P` | Pause / resume the stream |
| `Ctrl/Cmd+C` | Copy the selected line (list focused) |
| Up / Down / PgUp / PgDn / Home / End | Move the selection |

Scrolling up turns **Follow** off; click *Follow* in the status bar to jump back to the live end.

## MCP server (remote control)

The app embeds a [Model Context Protocol](https://modelcontextprotocol.io) server so an AI client or script can drive
the running window. Messages are newline-delimited JSON-RPC 2.0 on both transports.

| Flag | Transport |
| --- | --- |
| `--mcp-stdio` | The client launches the app and talks over its stdin/stdout. |
| `--mcp-tcp[=HOST:PORT]` | Listens on TCP (default `127.0.0.1:7878`); any number of clients. |

The flags can be combined and can be followed by a file to open: `log-viewer --mcp-tcp session.log`.

```sh
# Claude Code (stdio)
claude mcp add log-viewer -- "C:\Program Files\Log Viewer\log-viewer.exe" --mcp-stdio
# Quick TCP check
log-viewer --mcp-tcp 127.0.0.1:7878
```

**Security:** the TCP transport has no authentication and the tools can read and write files the app can access.
Keep it on loopback (the default); only bind to another interface on a network you trust.

Tools: `get_state`, `list_devices`, `connect_device`, `start_stream`, `stop_stream`, `set_paused`, `open_file`,
`save_file`, `clear_logs`, `set_filter` (query / regex / case_sensitive / levels), `get_logs` (offset, limit, from_end),
`select_line`, `set_theme`, `set_follow`. Line indexes refer to the currently *filtered* list.

### Settings

Stored in `%APPDATA%\LogViewer\settings.json` (Windows), `~/.config/LogViewer/settings.json` (Linux) or
`~/Library/Application Support/LogViewer/settings.json` (macOS). Set `"adb_path"` there if `adb` is not auto-detected.

## Building

```sh
cargo run --release            # run
cargo test                     # parser / filter / adb tests
```

Linux needs the GPUI system packages (see `.github/workflows/release.yml`). macOS needs Xcode (with the Metal
toolchain). Windows needs the MSVC build tools. Linux also needs a Vulkan-capable GPU driver at run time.

## Installers

Installers are produced with [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) and must be built on
their own OS - the GitHub Actions workflow in `.github/workflows/release.yml` does this for all platforms
(push a `v*` tag, or run it manually).

```sh
cargo install cargo-packager --locked
cargo packager --release --formats nsis,wix        # Windows: .exe setup + .msi
cargo packager --release --formats deb,appimage    # Linux
cargo packager --release --formats dmg             # macOS
```

## Layout

| File | Purpose |
| --- | --- |
| `src/main.rs` | window, key bindings, menus |
| `src/app.rs` | main view (toolbar, filters, list, detail, status bar) |
| `src/log.rs` | line parser, levels, filter |
| `src/adb.rs` | adb discovery, device list, logcat streaming |
| `src/input.rs` | single-line text input widget |
| `src/theme.rs` | light/dark palettes, persisted settings |
