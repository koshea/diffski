//! Filename filtering for the `/` search box.
//!
//! Grammar: the query splits on commas into terms; blank terms are ignored. A
//! leading `!` makes a term an exclusion. A term containing `*` or `?` is a
//! glob; anything else is a case-insensitive substring match on the
//! repo-relative path (the original behavior, preserved for muscle memory).
//!
//! Globs are case-insensitive. `**` matches across `/`, `*` and `?` do not. A
//! glob containing no `/` is matched against the filename only; one containing
//! `/` is matched against the full repo-relative path.
//!
//! A path passes when it matches at least one include term (or there are no
//! include terms) and matches no exclude term.

pub struct Query {
    includes: Vec<Term>,
    excludes: Vec<Term>,
}

enum Term {
    /// Lowercased needle, matched against the lowercased full path.
    Substring(String),
    Glob {
        tokens: Vec<Tok>,
        /// Pattern contained `/`: match the full path, not just the filename.
        full_path: bool,
    },
}

enum Tok {
    /// Any run of chars except `/`.
    Star,
    /// Any run of chars including `/`.
    DoubleStar,
    /// Exactly one char except `/`.
    QMark,
    Lit(char),
}

impl Query {
    pub fn parse(input: &str) -> Query {
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        for raw in input.split(',') {
            let raw = raw.trim();
            let (neg, body) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, raw),
            };
            if body.is_empty() {
                continue;
            }
            let term = Term::parse(body);
            if neg {
                excludes.push(term);
            } else {
                includes.push(term);
            }
        }
        Query { includes, excludes }
    }

    pub fn matches(&self, path: &str) -> bool {
        (self.includes.is_empty() || self.includes.iter().any(|t| t.matches(path)))
            && !self.excludes.iter().any(|t| t.matches(path))
    }
}

impl Term {
    fn parse(body: &str) -> Term {
        if !body.contains(['*', '?']) {
            return Term::Substring(body.to_lowercase());
        }
        let full_path = body.contains('/');
        let lower = body.to_lowercase();
        let mut tokens = Vec::new();
        let mut chars = lower.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' => {
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        tokens.push(Tok::DoubleStar);
                    } else {
                        tokens.push(Tok::Star);
                    }
                }
                '?' => tokens.push(Tok::QMark),
                c => tokens.push(Tok::Lit(c)),
            }
        }
        Term::Glob { tokens, full_path }
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Term::Substring(needle) => path.to_lowercase().contains(needle),
            Term::Glob { tokens, full_path } => {
                let target = if *full_path {
                    path
                } else {
                    path.rsplit('/').next().unwrap_or(path)
                };
                let text: Vec<char> = target.to_lowercase().chars().collect();
                glob_match(tokens, &text)
            }
        }
    }
}

/// Backtracking glob match of `tokens` against `text` (both already
/// lowercased). Patterns are short, so plain recursion is plenty fast.
fn glob_match(tokens: &[Tok], text: &[char]) -> bool {
    match tokens.split_first() {
        None => text.is_empty(),
        Some((Tok::Lit(c), rest)) => text.first() == Some(c) && glob_match(rest, &text[1..]),
        Some((Tok::QMark, rest)) => {
            matches!(text.first(), Some(&c) if c != '/') && glob_match(rest, &text[1..])
        }
        Some((Tok::Star, rest)) => (0..=text.len())
            .take_while(|&k| k == 0 || text[k - 1] != '/')
            .any(|k| glob_match(rest, &text[k..])),
        Some((Tok::DoubleStar, rest)) => (0..=text.len()).any(|k| glob_match(rest, &text[k..])),
    }
}

#[cfg(test)]
mod tests {
    use super::Query;

    fn matches(query: &str, path: &str) -> bool {
        Query::parse(query).matches(path)
    }

    #[test]
    fn empty_query_matches_everything() {
        assert!(matches("", "src/app.rs"));
        assert!(matches("  ,  ", "src/app.rs"));
    }

    #[test]
    fn plain_terms_are_case_insensitive_substrings_on_the_full_path() {
        assert!(matches("app", "src/app.rs"));
        assert!(matches("APP", "src/app.rs"));
        assert!(matches("src/a", "src/app.rs"));
        assert!(!matches("xyz", "src/app.rs"));
    }

    #[test]
    fn comma_separated_terms_or_together() {
        assert!(matches("app, git", "src/git.rs"));
        assert!(matches("app, git", "src/app.rs"));
        assert!(!matches("app, git", "src/ui.rs"));
    }

    #[test]
    fn bang_excludes() {
        assert!(!matches("!test", "src/test_util.rs"));
        assert!(matches("!test", "src/app.rs"));
    }

    #[test]
    fn includes_and_excludes_compose() {
        // *.rs files except anything with "test" in the name.
        assert!(matches("*.rs, !test", "src/app.rs"));
        assert!(!matches("*.rs, !test", "src/test_util.rs"));
        assert!(!matches("*.rs, !test", "docs/notes.md"));
    }

    #[test]
    fn excluding_globs_works() {
        assert!(!matches("!*.md", "README.md"));
        assert!(matches("!*.md", "src/app.rs"));
    }

    #[test]
    fn lone_bang_is_a_noop() {
        assert!(matches("!", "src/app.rs"));
        assert!(matches("app, !", "src/app.rs"));
    }

    #[test]
    fn glob_without_slash_matches_filename_only() {
        assert!(matches("*.rs", "src/app.rs"));
        assert!(matches("app.*", "src/app.rs"));
        // The filename is "app.rs" — a filename glob never sees "src".
        assert!(!matches("src*", "src/app.rs"));
    }

    #[test]
    fn glob_with_slash_matches_full_path() {
        assert!(matches("src/*.rs", "src/app.rs"));
        // `*` must not cross a directory separator...
        assert!(!matches("src/*.rs", "src/webview/tree.rs"));
        // ...but `**` does.
        assert!(matches("src/**.rs", "src/webview/tree.rs"));
        assert!(matches("src/**/tree.rs", "src/webview/tree.rs"));
    }

    #[test]
    fn question_mark_matches_exactly_one_char() {
        assert!(matches("a?p.rs", "src/app.rs"));
        assert!(matches("app.r?", "src/app.rs"));
        assert!(!matches("app.r?", "src/app.r"));
    }

    #[test]
    fn globs_are_case_insensitive() {
        assert!(matches("*.RS", "src/app.rs"));
        assert!(matches("APP*", "src/App.rs"));
    }
}
