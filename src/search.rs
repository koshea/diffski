//! In-diff content search: find a needle in the added/deleted lines of raw
//! git diffs, and (in `locate_matches`, Task 2) locate each hit in the
//! delta-rendered output via content-anchored alignment.
//!
//! Matching is literal with smartcase: an all-lowercase needle matches
//! case-insensitively (ASCII), any uppercase char makes it exact.

/// Remove ANSI escape sequences (the raw diffs are produced with
/// `--color=always`). Handles CSI sequences (`ESC [ … <final>`); any other
/// escape is dropped along with the character that follows it.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // `next()` swallows the escape's follower either way; only CSI
        // sequences (`ESC [ … <final>`) have more to skip.
        if chars.next() == Some('[') {
            for f in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&f) {
                    break;
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Add,
    Del,
    Context,
}

/// One body line of a unified diff (prefix stripped).
#[derive(Debug, Clone, PartialEq)]
pub struct BodyLine {
    pub text: String,
    pub kind: LineKind,
}

/// Extract the body lines (added / deleted / context) from an ANSI-stripped
/// unified diff. File headers (`--- `/`+++ `, which appear only before the
/// first `@@` of a file section), hunk headers, and `\ No newline` markers
/// are excluded.
pub fn body_lines(diff: &str) -> Vec<BodyLine> {
    let mut out = Vec::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if line.starts_with("diff ") {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            out.push(BodyLine {
                text: rest.to_string(),
                kind: LineKind::Add,
            });
        } else if let Some(rest) = line.strip_prefix('-') {
            out.push(BodyLine {
                text: rest.to_string(),
                kind: LineKind::Del,
            });
        } else if let Some(rest) = line.strip_prefix(' ') {
            out.push(BodyLine {
                text: rest.to_string(),
                kind: LineKind::Context,
            });
        }
        // Anything else inside a hunk (e.g. "\ No newline...") is skipped.
    }
    out
}

/// Which line kinds a content search inspects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Both,
    Add,
    Del,
}

impl Scope {
    pub fn cycle(self) -> Scope {
        match self {
            Scope::Both => Scope::Add,
            Scope::Add => Scope::Del,
            Scope::Del => Scope::Both,
        }
    }
    pub fn glyph(self) -> &'static str {
        match self {
            Scope::Both => "±",
            Scope::Add => "+",
            Scope::Del => "−",
        }
    }
    pub fn admits(self, kind: LineKind) -> bool {
        match (self, kind) {
            (_, LineKind::Context) => false,
            (Scope::Both, _) => true,
            (Scope::Add, LineKind::Add) => true,
            (Scope::Del, LineKind::Del) => true,
            _ => false,
        }
    }
}

/// Char-offset ranges of every smartcase occurrence of `needle` in `hay`.
pub fn find_matches(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let exact = needle.chars().any(|c| c.is_uppercase());
    let h: Vec<char> = hay.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    if n.len() > h.len() {
        return Vec::new();
    }
    let eq = |a: char, b: char| {
        if exact {
            a == b
        } else {
            a.eq_ignore_ascii_case(&b)
        }
    };
    let mut out = Vec::new();
    for start in 0..=(h.len() - n.len()) {
        if (0..n.len()).all(|k| eq(h[start + k], n[k])) {
            out.push((start, start + n.len()));
        }
    }
    out
}

/// Total scope-admitted needle occurrences in an ANSI-stripped raw diff.
pub fn count_matches(raw: &str, needle: &str, scope: Scope) -> usize {
    body_lines(raw)
        .iter()
        .filter(|b| scope.admits(b.kind))
        .map(|b| find_matches(&b.text, needle).len())
        .sum()
}

/// Split the search box text into `(content needle, filename query)`.
/// A `file:` prefix (any case) routes the remainder to the filename filter;
/// anything else is a content search. Never both.
pub fn split_query(q: &str) -> (Option<String>, String) {
    let q = q.trim();
    if q.is_empty() {
        return (None, String::new());
    }
    if let Some(prefix) = q.get(..5)
        && prefix.eq_ignore_ascii_case("file:")
    {
        return (None, q[5..].trim().to_string());
    }
    (Some(q.to_string()), String::new())
}

/// A needle occurrence located in the rendered output of one file's diff.
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    /// Index into the rendered (pre-wrap) lines of the file's section.
    pub line: usize,
    /// Char-column range of the needle within that rendered line.
    pub cols: (usize, usize),
    pub kind: LineKind,
    /// False when the raw line couldn't be found in the rendered output;
    /// such matches are counted and navigable but not highlighted.
    pub aligned: bool,
}

/// Locate every smartcase match of `needle` (within `scope`) in the rendered
/// lines of one file's diff, by content-anchored alignment: every body line
/// (including context, which never matches) anchors to the next rendered line
/// containing its tab-expanded content, advancing a forward-only cursor. This
/// assumes only that the renderer preserves line content.
pub fn locate_matches(
    raw: &str,
    rendered: &[String],
    needle: &str,
    scope: Scope,
    tab_width: u16,
) -> Vec<Located> {
    let tab = " ".repeat(tab_width.max(1) as usize);
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for body in body_lines(raw) {
        let expanded = body.text.replace('\t', &tab);
        let found = rendered
            .get(cursor..)
            .unwrap_or(&[])
            .iter()
            .position(|r| r.contains(&expanded))
            .map(|off| cursor + off);
        let hits = if scope.admits(body.kind) {
            find_matches(&expanded, needle)
        } else {
            Vec::new()
        };
        match found {
            Some(line) => {
                cursor = line + 1;
                if hits.is_empty() {
                    continue;
                }
                // Char offset of the content within the rendered line (the
                // line-number gutter the renderer prepends).
                let r = &rendered[line];
                let byte = r.find(&expanded).unwrap_or(0);
                let gutter = r[..byte].chars().count();
                for (cs, ce) in hits {
                    out.push(Located {
                        line,
                        cols: (gutter + cs, gutter + ce),
                        kind: body.kind,
                        aligned: true,
                    });
                }
            }
            None => {
                // Don't advance the cursor: later body lines may still align.
                for _ in hits {
                    out.push(Located {
                        line: 0,
                        cols: (0, 0),
                        kind: body.kind,
                        aligned: false,
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/f.rs b/f.rs
index 0000000..1111111 100644
--- a/f.rs
+++ b/f.rs
@@ -1,3 +1,3 @@ fn top()
 context value
-old value
+new value
\\ No newline at end of file
";

    #[test]
    fn strip_ansi_removes_sgr_sequences() {
        assert_eq!(strip_ansi("\u{1b}[32m+foo\u{1b}[0m"), "+foo");
        assert_eq!(strip_ansi("\u{1b}[1;38;2;10;20;30mX\u{1b}[m"), "X");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn body_lines_classifies_and_skips_headers() {
        let body = body_lines(DIFF);
        assert_eq!(
            body,
            vec![
                BodyLine {
                    text: "context value".into(),
                    kind: LineKind::Context
                },
                BodyLine {
                    text: "old value".into(),
                    kind: LineKind::Del
                },
                BodyLine {
                    text: "new value".into(),
                    kind: LineKind::Add
                },
            ]
        );
    }

    #[test]
    fn body_lines_handles_deleted_line_starting_with_dashes() {
        // Inside a hunk, "--x" is a deletion of "-x", not a file header.
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n--x\n+y\n";
        let body = body_lines(diff);
        assert_eq!(
            body[0],
            BodyLine {
                text: "-x".into(),
                kind: LineKind::Del
            }
        );
        assert_eq!(
            body[1],
            BodyLine {
                text: "y".into(),
                kind: LineKind::Add
            }
        );
    }

    #[test]
    fn smartcase_matching() {
        // all-lowercase needle: case-insensitive
        assert_eq!(
            find_matches("Old VALUE and value", "value"),
            vec![(4, 9), (14, 19)]
        );
        // uppercase in needle: exact
        assert_eq!(find_matches("Old VALUE and value", "VALUE"), vec![(4, 9)]);
        assert!(find_matches("abc", "").is_empty());
        assert!(find_matches("ab", "abc").is_empty());
    }

    #[test]
    fn find_matches_returns_char_offsets() {
        // "héllo " is 6 chars; byte offsets would differ.
        assert_eq!(find_matches("héllo world", "world"), vec![(6, 11)]);
    }

    #[test]
    fn count_matches_respects_scope() {
        assert_eq!(count_matches(DIFF, "value", Scope::Both), 2); // del + add, not context
        assert_eq!(count_matches(DIFF, "value", Scope::Add), 1);
        assert_eq!(count_matches(DIFF, "value", Scope::Del), 1);
        assert_eq!(count_matches(DIFF, "context", Scope::Both), 0);
    }

    #[test]
    fn split_query_routes_file_prefix() {
        assert_eq!(
            split_query("needle"),
            (Some("needle".into()), String::new())
        );
        assert_eq!(
            split_query("file:*.rs, !test"),
            (None, "*.rs, !test".into())
        );
        assert_eq!(split_query("FILE:app"), (None, "app".into()));
        assert_eq!(split_query(""), (None, String::new()));
        assert_eq!(split_query("  "), (None, String::new()));
        assert_eq!(split_query("file:"), (None, String::new()));
    }

    // Delta-like rendered output: header box, hunk decoration, then body
    // lines with a "N: " line-number gutter.
    fn rendered() -> Vec<String> {
        vec![
            "f.rs".to_string(),
            "────".to_string(),
            "1: context value".to_string(),
            "2: old value".to_string(),
            "2: new value".to_string(),
        ]
    }

    #[test]
    fn locate_finds_rows_and_gutter_offset_columns() {
        let m = locate_matches(DIFF, &rendered(), "value", Scope::Both, 4);
        assert_eq!(m.len(), 2);
        // "3: " gutter = 3 chars; "value" at chars 4..9 of "old value".
        assert_eq!(
            m[0],
            Located {
                line: 3,
                cols: (7, 12),
                kind: LineKind::Del,
                aligned: true
            }
        );
        assert_eq!(
            m[1],
            Located {
                line: 4,
                cols: (7, 12),
                kind: LineKind::Add,
                aligned: true
            }
        );
    }

    #[test]
    fn locate_respects_scope() {
        let m = locate_matches(DIFF, &rendered(), "value", Scope::Add, 4);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 4);
    }

    #[test]
    fn context_lines_consume_rendered_lines_so_duplicates_align_correctly() {
        // Context "foo" precedes an added "foo": the added line must anchor
        // to the SECOND rendered "foo", proving context lines advance the
        // alignment cursor even though they never match.
        let raw = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n foo\n+foo\n";
        let rendered = vec!["1: foo".to_string(), "2: foo".to_string()];
        let m = locate_matches(raw, &rendered, "foo", Scope::Add, 4);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 1);
    }

    #[test]
    fn locate_expands_tabs_like_delta() {
        let raw = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n+\tfoo\n";
        let rendered = vec!["1:     foo".to_string()]; // tab -> 4 spaces
        let m = locate_matches(raw, &rendered, "foo", Scope::Both, 4);
        assert_eq!(
            m[0],
            Located {
                line: 0,
                cols: (7, 10),
                kind: LineKind::Add,
                aligned: true
            }
        );
    }

    #[test]
    fn unalignable_lines_degrade_gracefully() {
        // Rendered output missing the deleted line entirely: the del match is
        // reported unaligned; the add match still aligns.
        let rendered = vec!["1: context value".to_string(), "2: new value".to_string()];
        let m = locate_matches(DIFF, &rendered, "value", Scope::Both, 4);
        assert_eq!(m.len(), 2);
        assert!(!m[0].aligned);
        assert_eq!(
            m[1],
            Located {
                line: 1,
                cols: (7, 12),
                kind: LineKind::Add,
                aligned: true
            }
        );
    }

    #[test]
    fn locate_gutter_offset_is_char_based() {
        let raw = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n+value\n";
        // Multibyte gutter: "→ " is 2 chars but 4 bytes.
        let rendered = vec!["→ value".to_string()];
        let m = locate_matches(raw, &rendered, "value", Scope::Both, 4);
        assert_eq!(m[0].cols, (2, 7));
    }
}
