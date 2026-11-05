//! Go's path.Match: shell patterns with *, ?, [class] and \ escapes, where
//! neither * nor ? crosses a /.

/// Reports whether name matches pattern, or an error for a malformed pattern.
pub fn matches(pattern: &str, name: &str) -> Result<bool, ()> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let ok = match_at(&p, &n)?;
    // Go reports a bad pattern even when an early mismatch stopped the scan.
    check(&p)?;
    Ok(ok)
}

fn match_at(p: &[char], n: &[char]) -> Result<bool, ()> {
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some((pi, ni));
                    pi += 1;
                    continue;
                }
                '?' if ni < n.len() && n[ni] != '/' => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                '[' if ni < n.len() => {
                    let (ok, next) = class(p, pi, n[ni])?;
                    if ok {
                        pi = next;
                        ni += 1;
                        continue;
                    }
                }
                '\\' => {
                    if pi + 1 >= p.len() {
                        return Err(());
                    }
                    if ni < n.len() && n[ni] == p[pi + 1] {
                        pi += 2;
                        ni += 1;
                        continue;
                    }
                }
                c if c != '?' && c != '[' && ni < n.len() && n[ni] == c => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        } else if ni == n.len() {
            return Ok(true);
        }
        match star {
            Some((sp, sn)) if sn < n.len() && n[sn] != '/' => {
                star = Some((sp, sn + 1));
                pi = sp + 1;
                ni = sn + 1;
            }
            _ => return Ok(false),
        }
    }
}

/// Matches c against the class starting at p[start] == '['; returns whether it
/// matched and the index after the closing ].
fn class(p: &[char], start: usize, c: char) -> Result<(bool, usize), ()> {
    let mut i = start + 1;
    let negate = i < p.len() && p[i] == '^';
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        if i >= p.len() {
            return Err(());
        }
        if p[i] == ']' && !first {
            i += 1;
            break;
        }
        first = false;
        let (lo, next) = class_char(p, i)?;
        i = next;
        let mut hi = lo;
        if i < p.len() && p[i] == '-' {
            let (h, next) = class_char(p, i + 1)?;
            hi = h;
            i = next;
        }
        if lo <= c && c <= hi {
            matched = true;
        }
    }
    Ok((matched != negate, i))
}

fn class_char(p: &[char], i: usize) -> Result<(char, usize), ()> {
    match p.get(i) {
        None | Some('-') | Some(']') => Err(()),
        Some('\\') => p.get(i + 1).map(|c| (*c, i + 2)).ok_or(()),
        Some(c) => Ok((*c, i + 1)),
    }
}

fn check(p: &[char]) -> Result<(), ()> {
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '\\' => {
                if i + 1 >= p.len() {
                    return Err(());
                }
                i += 2;
            }
            '[' => i = class(p, i, '\0')?.1,
            _ => i += 1,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn matches_like_go_path_match() {
        assert_eq!(matches("release-*", "release-mac"), Ok(true));
        assert_eq!(matches("release", "release-mac"), Ok(false));
        assert_eq!(matches("a?c", "abc"), Ok(true));
        assert_eq!(matches("*", "a/b"), Ok(false));
        assert_eq!(matches("[a-c]x", "bx"), Ok(true));
        assert_eq!(matches("[^a-c]x", "bx"), Ok(false));
        assert_eq!(matches("\\*", "*"), Ok(true));
        assert!(matches("[", "").is_err());
        assert!(matches("a\\", "").is_err());
        assert!(matches("[]", "").is_err());
    }
}
