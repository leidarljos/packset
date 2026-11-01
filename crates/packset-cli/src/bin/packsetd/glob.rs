//! Shell-style name matching, the way `fnmatch` does it.
//!
//! The ignore files a repository already carries are written against that
//! grammar, so matching them any other way would silently include or exclude
//! the wrong paths. `*` spans separators here, which is `fnmatch` rather than
//! gitignore, and is what the patterns were tested against.

/// Whether `name` matches `pattern`.
#[must_use]
pub fn matches(name: &str, pattern: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    is_match(&n, 0, &p, 0)
}

fn is_match(name: &[char], mut ni: usize, pat: &[char], mut pi: usize) -> bool {
    // Backtracking point for the most recent `*`, which is what keeps this
    // linear in practice without a regex engine.
    let mut star: Option<(usize, usize)> = None;
    while ni < name.len() {
        if pi < pat.len() {
            match pat[pi] {
                '*' => {
                    star = Some((pi, ni));
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                '[' => {
                    if let Some((end, hit)) = class_match(pat, pi, name[ni]) {
                        if hit {
                            pi = end + 1;
                            ni += 1;
                            continue;
                        }
                    } else if pat[pi] == name[ni] {
                        // An unterminated `[` is a literal, which is what
                        // fnmatch does with it.
                        pi += 1;
                        ni += 1;
                        continue;
                    }
                }
                ch if ch == name[ni] => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((sp, sn)) => {
                pi = sp + 1;
                ni = sn + 1;
                star = Some((sp, sn + 1));
            }
            None => return false,
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// The end index of a `[...]` class and whether `ch` is in it.
fn class_match(pat: &[char], open: usize, ch: char) -> Option<(usize, bool)> {
    let mut i = open + 1;
    let negated = i < pat.len() && (pat[i] == '!' || pat[i] == '^');
    if negated {
        i += 1;
    }
    let first = i;
    let mut hit = false;
    while i < pat.len() {
        if pat[i] == ']' && i > first {
            return Some((i, hit != negated));
        }
        if i + 2 < pat.len() && pat[i + 1] == '-' && pat[i + 2] != ']' {
            if pat[i] <= ch && ch <= pat[i + 2] {
                hit = true;
            }
            i += 3;
        } else {
            if pat[i] == ch {
                hit = true;
            }
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_spans_a_separator() {
        // fnmatch, not gitignore: the ignore files were written against this.
        assert!(matches("a/b/c.rs", "*.rs"));
        assert!(matches("target/debug/x", "target/*"));
        assert!(!matches("src/x.py", "*.rs"));
    }

    #[test]
    fn a_question_mark_takes_one_character() {
        assert!(matches("ab", "a?"));
        assert!(!matches("abc", "a?"));
        assert!(!matches("a", "a?"));
    }

    #[test]
    fn a_class_matches_a_set_and_a_range() {
        assert!(matches("a", "[abc]"));
        assert!(matches("b", "[a-c]"));
        assert!(!matches("d", "[a-c]"));
        assert!(matches("d", "[!a-c]"));
        assert!(matches("d", "[^a-c]"));
    }

    #[test]
    fn a_bare_name_matches_itself_and_nothing_else() {
        assert!(matches("build", "build"));
        assert!(!matches("builder", "build"));
        assert!(!matches("build", "builder"));
    }

    #[test]
    fn stars_at_both_ends_find_the_middle() {
        assert!(matches("a/node_modules/b", "*node_modules*"));
        assert!(matches("", "*"));
        assert!(matches("anything", "*"));
    }

    #[test]
    fn a_backtracking_pattern_terminates() {
        assert!(!matches(&"a".repeat(40), "*a*a*a*b"));
        assert!(matches("aaaab", "*a*a*a*b"));
    }
}
