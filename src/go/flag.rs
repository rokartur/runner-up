//! Go's flag package, as runner-up's commands parse their flags: -name or
//! --name, -name=value or -name value, bool flags without a value, parsing
//! stops at the first argument that is not a flag or after "--".

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Dur(Duration),
    /// A flag that may be given many times, each adding one value.
    List(Vec<String>),
}

impl Value {
    fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "string",
            Value::Int(_) => "int",
            Value::Bool(_) => "",
            Value::Dur(_) => "duration",
            Value::List(_) => "value",
        }
    }

    fn default_text(&self) -> Option<String> {
        match self {
            Value::Str(s) if !s.is_empty() => Some(format!("{s:?}")),
            Value::Int(n) if *n != 0 => Some(n.to_string()),
            Value::Bool(true) => Some("true".into()),
            Value::Dur(d) if !d.is_zero() => Some(super::duration::format(*d)),
            _ => None,
        }
    }
}

struct Flag {
    usage: String,
    value: Value,
}

pub enum ParseError {
    Help,
    Bad,
}

pub struct FlagSet {
    pub name: String,
    flags: BTreeMap<String, Flag>,
    args: Vec<String>,
    /// Replaces the default usage text, as a Go FlagSet's Usage function does.
    pub usage: Option<Box<dyn Fn(&FlagSet, &mut dyn Write)>>,
}

impl FlagSet {
    pub fn new(name: &str) -> Self {
        FlagSet { name: name.into(), flags: BTreeMap::new(), args: vec![], usage: None }
    }

    fn define(&mut self, name: &str, value: Value, usage: &str) {
        self.flags.insert(name.into(), Flag { usage: usage.into(), value });
    }

    pub fn string(&mut self, name: &str, default: &str, usage: &str) {
        self.define(name, Value::Str(default.into()), usage)
    }
    pub fn int(&mut self, name: &str, default: i64, usage: &str) {
        self.define(name, Value::Int(default), usage)
    }
    pub fn bool(&mut self, name: &str, default: bool, usage: &str) {
        self.define(name, Value::Bool(default), usage)
    }
    pub fn duration(&mut self, name: &str, default: Duration, usage: &str) {
        self.define(name, Value::Dur(default), usage)
    }
    pub fn list(&mut self, name: &str, usage: &str) {
        self.define(name, Value::List(vec![]), usage)
    }

    pub fn get_str(&self, name: &str) -> String {
        match &self.flags[name].value {
            Value::Str(s) => s.clone(),
            v => panic!("flag {name} is {v:?}, not a string"),
        }
    }
    pub fn get_int(&self, name: &str) -> i64 {
        match &self.flags[name].value {
            Value::Int(n) => *n,
            v => panic!("flag {name} is {v:?}, not an int"),
        }
    }
    pub fn get_bool(&self, name: &str) -> bool {
        match &self.flags[name].value {
            Value::Bool(b) => *b,
            v => panic!("flag {name} is {v:?}, not a bool"),
        }
    }
    pub fn get_dur(&self, name: &str) -> Duration {
        match &self.flags[name].value {
            Value::Dur(d) => *d,
            v => panic!("flag {name} is {v:?}, not a duration"),
        }
    }
    pub fn get_list(&self, name: &str) -> Vec<String> {
        match &self.flags[name].value {
            Value::List(l) => l.clone(),
            v => panic!("flag {name} is {v:?}, not a list"),
        }
    }

    /// The arguments left after the flags.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Parses args as Go's flag.FlagSet.Parse with ContinueOnError: errors
    /// and -h print to standard error.
    pub fn parse(&mut self, args: &[String]) -> Result<(), ParseError> {
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a.len() < 2 || !a.starts_with('-') {
                break;
            }
            let mut name = &a[1..];
            if let Some(n) = name.strip_prefix('-') {
                if n.is_empty() {
                    i += 1;
                    break;
                }
                name = n;
            }
            if name.starts_with('-') || name.starts_with('=') {
                return self.fail(&format!("bad flag syntax: {a}"));
            }
            i += 1;
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (name, None),
            };
            let Some(flag) = self.flags.get(name) else {
                if name == "help" || name == "h" {
                    self.print_usage();
                    return Err(ParseError::Help);
                }
                return self.fail(&format!("flag provided but not defined: -{name}"));
            };
            let is_bool = matches!(flag.value, Value::Bool(_));
            let text = match inline {
                Some(v) => v,
                None if is_bool => "true".into(),
                None => {
                    if i >= args.len() {
                        return self.fail(&format!("flag needs an argument: -{name}"));
                    }
                    i += 1;
                    args[i - 1].clone()
                }
            };
            if let Err(e) = self.set(name, &text) {
                let msg = if is_bool {
                    format!("invalid boolean value {text:?} for -{name}: {e}")
                } else {
                    format!("invalid value {text:?} for flag -{name}: {e}")
                };
                return self.fail(&msg);
            }
        }
        self.args = args[i..].to_vec();
        Ok(())
    }

    fn set(&mut self, name: &str, text: &str) -> Result<(), String> {
        let flag = self.flags.get_mut(name).expect("defined flag");
        match &mut flag.value {
            Value::Str(s) => *s = text.into(),
            Value::Int(n) => *n = parse_int(text).ok_or("parse error")?,
            Value::Bool(b) => *b = parse_bool(text).ok_or("parse error")?,
            Value::Dur(d) => *d = super::duration::parse(text).map_err(|_| "parse error")?,
            Value::List(l) => l.push(text.into()),
        }
        Ok(())
    }

    fn fail(&self, msg: &str) -> Result<(), ParseError> {
        let _ = writeln!(std::io::stderr(), "{msg}");
        self.print_usage();
        Err(ParseError::Bad)
    }

    pub fn print_usage(&self) {
        let mut err = std::io::stderr();
        match &self.usage {
            Some(f) => f(self, &mut err),
            None => {
                let _ = writeln!(err, "Usage of {}:", self.name);
                self.print_defaults(&mut err);
            }
        }
    }

    /// Lists the flags as Go's PrintDefaults does.
    pub fn print_defaults(&self, w: &mut dyn Write) {
        for (name, flag) in &self.flags {
            let (kind, usage) = unquote_usage(&flag.value, &flag.usage);
            let mut line = format!("  -{name}");
            if !kind.is_empty() {
                line += " ";
                line += &kind;
            }
            if line.len() <= 4 {
                line += "\t";
            } else {
                line += "\n    \t";
            }
            line += &usage.replace('\n', "\n    \t");
            if let Some(d) = flag.value.default_text() {
                line += &format!(" (default {d})");
            }
            let _ = writeln!(w, "{line}");
        }
    }
}

/// Go's flag.UnquoteUsage: a `backquoted` word in the usage names the value.
fn unquote_usage(value: &Value, usage: &str) -> (String, String) {
    if let Some(start) = usage.find('`') {
        if let Some(len) = usage[start + 1..].find('`') {
            let name = &usage[start + 1..start + 1 + len];
            let text = format!("{}{}{}", &usage[..start], name, &usage[start + 2 + len..]);
            return (name.into(), text);
        }
    }
    (value.type_name().into(), usage.into())
}

fn parse_int(s: &str) -> Option<i64> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits = digits.replace('_', "");
    let (radix, body) = if let Some(h) = digits.strip_prefix("0x").or(digits.strip_prefix("0X")) {
        (16, h.to_string())
    } else if let Some(b) = digits.strip_prefix("0b").or(digits.strip_prefix("0B")) {
        (2, b.to_string())
    } else if let Some(o) = digits.strip_prefix("0o").or(digits.strip_prefix("0O")) {
        (8, o.to_string())
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, digits[1..].to_string())
    } else {
        (10, digits.clone())
    };
    let n = i64::from_str_radix(&body, radix).ok()?;
    Some(if neg { -n } else { n })
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_like_go_flag() {
        let mut fs = FlagSet::new("t");
        fs.string("config", "x", "setup file");
        fs.int("n", 1, "count");
        fs.bool("force", false, "force");
        fs.list("place", "place id");
        let ok = fs.parse(&args(&["--config=a", "-n", "3", "-force", "--place", "p1", "-place=p2", "rest", "-x"]));
        assert!(ok.is_ok());
        assert_eq!(fs.get_str("config"), "a");
        assert_eq!(fs.get_int("n"), 3);
        assert!(fs.get_bool("force"));
        assert_eq!(fs.get_list("place"), vec!["p1", "p2"]);
        assert_eq!(fs.args(), &["rest", "-x"]);

        let mut fs = FlagSet::new("t");
        fs.int("n", 0, "count");
        assert!(matches!(fs.parse(&args(&["-n", "x"])), Err(ParseError::Bad)));
        assert!(matches!(fs.parse(&args(&["-h"])), Err(ParseError::Help)));
        assert!(matches!(fs.parse(&args(&["-y"])), Err(ParseError::Bad)));
        assert!(fs.parse(&args(&["--", "-n"])).is_ok());
        assert_eq!(fs.args(), &["-n"]);
    }

    #[test]
    fn prints_defaults_like_go() {
        let mut fs = FlagSet::new("t");
        fs.string("config", "/a", "setup `FILE`");
        fs.bool("x", false, "short");
        let mut out = vec![];
        fs.print_defaults(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "  -config FILE\n    \tsetup FILE (default \"/a\")\n  -x\tshort\n"
        );
    }
}
