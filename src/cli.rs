//! Parses one command's flags the same way for every runner-up command: -h
//! shows the command's own flags and exits 0, a bad flag or an argument the
//! command does not take exits 2, and both are said on standard error.

use crate::go::flag::{FlagSet, ParseError};

/// Parses args into fs. Some(code) means the command ends with code: 0 after
/// -h, 2 after a bad flag, or 2 after arguments left over when positional is
/// false, as for a command that takes flags only.
pub fn parse(fs: &mut FlagSet, args: &[String], positional: bool) -> Option<i32> {
    match fs.parse(args) {
        Err(ParseError::Help) => return Some(0),
        Err(ParseError::Bad) => return Some(2),
        Ok(()) => {}
    }
    if !positional && !fs.args().is_empty() {
        eprintln!("{} takes no arguments, only flags; got {}", fs.name, fs.args().join(" "));
        return Some(2);
    }
    None
}

/// Reports whether args asks for help, as -h, -help, --help or help.
pub fn help(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("-h" | "-help" | "--help" | "help"))
}
