//! Small parts of Go's standard library whose exact behavior runner-up's
//! files, flags and output depend on.

pub mod duration;
pub mod flag;
pub mod pathmatch;
pub mod slog;
pub mod time;

/// Go's strconv.Quote, which %q writes for a string.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            '\n' => out += "\\n",
            '\r' => out += "\\r",
            '\t' => out += "\\t",
            '\x07' => out += "\\a",
            '\x08' => out += "\\b",
            '\x0c' => out += "\\f",
            '\x0b' => out += "\\v",
            c if (c as u32) < 0x20 || c == '\x7f' => out += &format!("\\x{:02x}", c as u32),
            c if c.is_control() => {
                if (c as u32) < 0x10000 {
                    out += &format!("\\u{:04x}", c as u32)
                } else {
                    out += &format!("\\U{:08x}", c as u32)
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Go's %v for a list of strings: [a b c].
pub fn list(items: &[impl AsRef<str>]) -> String {
    let parts: Vec<&str> = items.iter().map(|s| s.as_ref()).collect();
    format!("[{}]", parts.join(" "))
}

/// strings.Trim(s, set) == "": every character of s is in set.
pub fn only(s: &str, set: &str) -> bool {
    s.chars().all(|c| set.contains(c))
}
