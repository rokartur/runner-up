//! Quotes words for POSIX and Windows command lines.

/// Quotes s for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The characters a word may hold to reach a Windows program unchanged from
/// both cmd and PowerShell without quotes.
pub const WIN_PLAIN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-.:\\/=+";

/// Writes s so that cmd and PowerShell both hand it to the program as one
/// argument: plain words as they are, others in double quotes. Neither shell
/// has a quote that keeps every character, so check_win_word refuses the
/// characters that would still change inside double quotes.
pub fn win_quote(s: &str) -> String {
    if !s.is_empty() && crate::go::only(s, WIN_PLAIN) {
        return s.to_string();
    }
    format!("\"{s}\"")
}

/// Refuses a word win_quote cannot pass on unchanged: inside double quotes
/// cmd still expands %, PowerShell $ and `, and " ends the quote. The
/// program's own name must be plain: PowerShell does not run a quoted word.
pub fn check_win_word(s: &str, program: bool) -> Result<(), String> {
    let q = crate::go::quote(s);
    if program && (s.is_empty() || !crate::go::only(s, WIN_PLAIN)) {
        return Err(format!("{q} must be a plain program name or path, with no spaces, on a Windows machine"));
    }
    if s.contains(['"', '%', '$', '`', '\n', '\r']) || s.ends_with('\\') {
        return Err(format!("{q} cannot pass to a Windows machine's shell unchanged"));
    }
    Ok(())
}
