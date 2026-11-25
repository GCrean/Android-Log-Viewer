//! Log model, line parser and filter.

use regex::{Regex, RegexBuilder};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Level {
    Verbose = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
    Fatal = 5,
}

impl Level {
    pub const ALL: [Level; 6] = [
        Level::Verbose,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
        Level::Fatal,
    ];

    pub fn letter(self) -> &'static str {
        match self {
            Level::Verbose => "V",
            Level::Debug => "D",
            Level::Info => "I",
            Level::Warn => "W",
            Level::Error => "E",
            Level::Fatal => "F",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Verbose => "Verbose",
            Level::Debug => "Debug",
            Level::Info => "Info",
            Level::Warn => "Warn",
            Level::Error => "Error",
            Level::Fatal => "Fatal",
        }
    }

    fn from_letter(c: u8) -> Option<Level> {
        Some(match c {
            b'V' => Level::Verbose,
            b'D' => Level::Debug,
            b'I' => Level::Info,
            b'W' => Level::Warn,
            b'E' => Level::Error,
            b'F' | b'A' => Level::Fatal,
            _ => return None,
        })
    }

    fn from_word(w: &str) -> Option<Level> {
        Some(match w.to_ascii_uppercase().as_str() {
            "TRACE" | "VERBOSE" | "VERB" => Level::Verbose,
            "DEBUG" | "DBG" => Level::Debug,
            "INFO" | "NOTICE" => Level::Info,
            "WARN" | "WARNING" => Level::Warn,
            "ERROR" | "ERR" | "SEVERE" => Level::Error,
            "FATAL" | "CRITICAL" | "CRIT" | "PANIC" | "EMERG" | "ALERT" => Level::Fatal,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Default, Debug)]
struct Span(u32, u32);

/// One log line. The raw text is kept verbatim (so saving round-trips exactly)
/// and the structured fields are byte ranges into it.
#[derive(Clone, Debug)]
pub struct LogEntry {
    raw: Box<str>,
    pub level: Level,
    time: Span,
    pid: Span,
    tid: Span,
    tag: Span,
    msg_start: u32,
}

impl LogEntry {
    fn slice(&self, s: Span) -> &str {
        &self.raw[s.0 as usize..s.1 as usize]
    }
    pub fn raw(&self) -> &str {
        &self.raw
    }
    pub fn time(&self) -> &str {
        self.slice(self.time)
    }
    pub fn pid(&self) -> &str {
        self.slice(self.pid)
    }
    pub fn tid(&self) -> &str {
        self.slice(self.tid)
    }
    pub fn tag(&self) -> &str {
        self.slice(self.tag)
    }
    pub fn message(&self) -> &str {
        &self.raw[self.msg_start as usize..]
    }
}

/// Parses lines, carrying the previous level forward for lines that carry none
/// (stack-trace continuations etc.).
pub struct Parser {
    prev: Level,
}

impl Default for Parser {
    fn default() -> Self {
        Parser { prev: Level::Info }
    }
}

impl Parser {
    pub fn parse(&mut self, line: &str) -> LogEntry {
        let line = line.trim_end_matches(['\r', '\n']);
        let entry = parse_threadtime(line)
            .or_else(|| parse_brief(line))
            .unwrap_or_else(|| {
                let level = detect_level(line).unwrap_or(self.prev);
                LogEntry {
                    raw: line.into(),
                    level,
                    time: Span::default(),
                    pid: Span::default(),
                    tid: Span::default(),
                    tag: Span::default(),
                    msg_start: 0,
                }
            });
        self.prev = entry.level;
        entry
    }
}

/// Reads `[YYYY-]MM-DD HH:MM:SS.mmm PID TID L Tag: message` (logcat `-v threadtime`).
fn parse_threadtime(line: &str) -> Option<LogEntry> {
    let b = line.as_bytes();
    let mut pos = 0;
    let token = |pos: &mut usize| -> Option<(usize, usize)> {
        while *pos < b.len() && b[*pos] == b' ' {
            *pos += 1;
        }
        let start = *pos;
        while *pos < b.len() && b[*pos] != b' ' {
            *pos += 1;
        }
        (*pos > start).then_some((start, *pos))
    };
    let date = token(&mut pos)?;
    let time = token(&mut pos)?;
    let d = &line[date.0..date.1];
    let t = &line[time.0..time.1];
    let digits = |s: &str, sep: u8| s.bytes().all(|c| c.is_ascii_digit() || c == sep);
    if !(d.len() >= 5 && d.len() <= 10 && digits(d, b'-') && d.contains('-')) {
        return None;
    }
    if !(t.len() >= 8 && t.contains(':') && t.bytes().all(|c| c.is_ascii_digit() || c == b':' || c == b'.')) {
        return None;
    }
    let pid = token(&mut pos)?;
    let tid = token(&mut pos)?;
    let lvl = token(&mut pos)?;
    let all_digits = |s: (usize, usize)| line[s.0..s.1].bytes().all(|c| c.is_ascii_digit());
    if !all_digits(pid) || !all_digits(tid) || lvl.1 - lvl.0 != 1 {
        return None;
    }
    let level = Level::from_letter(b[lvl.0])?;
    // Tag runs up to the first ": " (padded with spaces).
    let rest = &line[lvl.1..];
    let sep = rest.find(": ").or_else(|| rest.strip_suffix(':').map(|r| r.len()));
    let (tag_start, tag_end, msg_start) = match sep {
        Some(i) => {
            let abs = lvl.1 + i;
            let tag_raw = &line[lvl.1..abs];
            let lead = tag_raw.len() - tag_raw.trim_start().len();
            (lvl.1 + lead, lvl.1 + tag_raw.trim_end().len().max(lead), (abs + 2).min(line.len()))
        }
        None => (line.len(), line.len(), line.len()),
    };
    Some(LogEntry {
        raw: line.into(),
        level,
        time: Span((date.0) as u32, time.1 as u32),
        pid: Span(pid.0 as u32, pid.1 as u32),
        tid: Span(tid.0 as u32, tid.1 as u32),
        tag: Span(tag_start as u32, tag_end.max(tag_start) as u32),
        msg_start: msg_start as u32,
    })
}

/// Reads `L/Tag( 1234): message` (logcat `-v brief`).
fn parse_brief(line: &str) -> Option<LogEntry> {
    let b = line.as_bytes();
    if b.len() < 4 || b[1] != b'/' {
        return None;
    }
    let level = Level::from_letter(b[0])?;
    let open = line.find('(')?;
    let close = line[open..].find("): ")? + open;
    let pid = line[open + 1..close].trim();
    if pid.is_empty() || !pid.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let pid_start = open + 1 + (line[open + 1..close].len() - line[open + 1..close].trim_start().len());
    Some(LogEntry {
        raw: line.into(),
        level,
        time: Span::default(),
        pid: Span(pid_start as u32, (pid_start + pid.len()) as u32),
        tid: Span::default(),
        tag: Span(2, open as u32),
        msg_start: (close + 3) as u32,
    })
}

/// Looks for a level keyword (`ERROR`, `[warn]`, ...) near the start of a free-form line.
fn detect_level(line: &str) -> Option<Level> {
    let head: String = line.chars().take(100).collect();
    let mut offset = 0;
    for word in head.split(|c: char| !c.is_ascii_alphabetic()) {
        if let Some(level) = Level::from_word(word) {
            let upper = word.chars().all(|c| c.is_ascii_uppercase());
            let pos = head[offset..].find(word).map(|p| p + offset).unwrap_or(0);
            if upper || pos < 40 {
                return Some(level);
            }
        }
        offset += word.len() + 1;
    }
    None
}

pub fn parse_text(text: &str) -> Vec<LogEntry> {
    let mut parser = Parser::default();
    text.lines().filter(|l| !l.is_empty()).map(|l| parser.parse(l)).collect()
}

#[derive(Clone)]
pub struct Filter {
    pub levels: [bool; 6],
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    compiled: Option<Regex>,
    pub error: Option<String>,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            levels: [true; 6],
            query: String::new(),
            regex: false,
            case_sensitive: false,
            compiled: None,
            error: None,
        }
    }
}

impl Filter {
    /// Recompile the search expression after `query`, `regex` or `case_sensitive` change.
    pub fn recompile(&mut self) {
        self.error = None;
        self.compiled = None;
        if self.query.is_empty() {
            return;
        }
        let pattern = if self.regex { self.query.clone() } else { regex::escape(&self.query) };
        match RegexBuilder::new(&pattern).case_insensitive(!self.case_sensitive).build() {
            Ok(re) => self.compiled = Some(re),
            Err(e) => self.error = Some(e.to_string().lines().last().unwrap_or("invalid regex").trim().to_string()),
        }
    }

    pub fn matches(&self, e: &LogEntry) -> bool {
        self.levels[e.level as usize] && self.compiled.as_ref().map_or(true, |re| re.is_match(e.raw()))
    }

    pub fn is_active(&self) -> bool {
        self.compiled.is_some() || self.levels.iter().any(|l| !l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threadtime() {
        let mut p = Parser::default();
        let e = p.parse("11-20 19:02:47.123  1234  5678 E AndroidRuntime: FATAL EXCEPTION: main");
        assert_eq!(e.level, Level::Error);
        assert_eq!(e.time(), "11-20 19:02:47.123");
        assert_eq!(e.pid(), "1234");
        assert_eq!(e.tid(), "5678");
        assert_eq!(e.tag(), "AndroidRuntime");
        assert_eq!(e.message(), "FATAL EXCEPTION: main");
    }

    #[test]
    fn threadtime_year_and_padded_tag() {
        let mut p = Parser::default();
        let e = p.parse("2026-11-20 19:02:47.123   12   34 W ActivityManager  : slow");
        assert_eq!(e.level, Level::Warn);
        assert_eq!(e.tag(), "ActivityManager");
        assert_eq!(e.message(), "slow");
    }

    #[test]
    fn brief() {
        let mut p = Parser::default();
        let e = p.parse("D/WifiHAL( 1234): hello");
        assert_eq!(e.level, Level::Debug);
        assert_eq!(e.tag(), "WifiHAL");
        assert_eq!(e.pid(), "1234");
        assert_eq!(e.message(), "hello");
    }

    #[test]
    fn freeform_and_inheritance() {
        let mut p = Parser::default();
        assert_eq!(p.parse("2026-01-01 [ERROR] boom").level, Level::Error);
        assert_eq!(p.parse("    at com.foo.Bar(Bar.java:1)").level, Level::Error);
        assert_eq!(p.parse("plain text").message(), "plain text");
    }

    #[test]
    fn filter() {
        let mut p = Parser::default();
        let e = p.parse("11-20 19:02:47.123  1  2 I Tag: Hello World");
        let mut f = Filter::default();
        f.query = "hello".into();
        f.recompile();
        assert!(f.matches(&e));
        f.case_sensitive = true;
        f.recompile();
        assert!(!f.matches(&e));
        f.case_sensitive = false;
        f.regex = true;
        f.query = "h.llo\\s+w".into();
        f.recompile();
        assert!(f.matches(&e));
        f.levels[Level::Info as usize] = false;
        assert!(!f.matches(&e));
        f.query = "(".into();
        f.recompile();
        assert!(f.error.is_some());
    }
}
