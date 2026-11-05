//! Go's log/slog text handler: `time=... level=INFO msg="..." key=value`,
//! the lines runner-up's logs are read by.

use std::fmt::Display;
use std::io::Write;
use std::sync::{Arc, Mutex};

pub type Attr<'a> = (&'a str, &'a dyn Display);

#[derive(Clone)]
pub struct Logger {
    out: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    prefix: String,
}

impl Logger {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Logger { out: Some(Arc::new(Mutex::new(out))), prefix: String::new() }
    }

    pub fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }

    pub fn stderr() -> Self {
        Self::new(Box::new(std::io::stderr()))
    }

    /// A logger that writes nothing, as slog.New(slog.NewTextHandler(io.Discard, nil)).
    pub fn discard() -> Self {
        Logger { out: None, prefix: String::new() }
    }

    /// A logger that adds attrs to every line, as slog.Logger.With.
    pub fn with(&self, attrs: &[Attr]) -> Self {
        let mut l = self.clone();
        l.prefix += &format_attrs(attrs);
        l
    }

    pub fn info(&self, msg: &str, attrs: &[Attr]) {
        self.log("INFO", msg, attrs)
    }
    pub fn warn(&self, msg: &str, attrs: &[Attr]) {
        self.log("WARN", msg, attrs)
    }
    pub fn error(&self, msg: &str, attrs: &[Attr]) {
        self.log("ERROR", msg, attrs)
    }

    fn log(&self, level: &str, msg: &str, attrs: &[Attr]) {
        let Some(out) = &self.out else { return };
        let time = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
        let line = format!("time={time} level={level} msg={}{}{}\n", value(msg), self.prefix, format_attrs(attrs));
        let mut w = out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.write_all(line.as_bytes());
        let _ = w.flush();
    }
}

fn format_attrs(attrs: &[Attr]) -> String {
    let mut s = String::new();
    for (k, v) in attrs {
        s += &format!(" {}={}", value(k), value(&v.to_string()));
    }
    s
}

/// Quotes a value as slog does: when empty, or holding a space, '=', '"' or
/// an unprintable character.
fn value(s: &str) -> String {
    let needs = s.is_empty() || s.chars().any(|c| c == ' ' || c == '=' || c == '"' || c.is_control() || c.is_whitespace());
    if needs { super::quote(s) } else { s.to_string() }
}

#[cfg(test)]
mod tests {
    use super::value;

    #[test]
    fn quotes_like_slog() {
        assert_eq!(value("plain"), "plain");
        assert_eq!(value("two words"), "\"two words\"");
        assert_eq!(value(""), "\"\"");
        assert_eq!(value("a=b"), "\"a=b\"");
    }
}
