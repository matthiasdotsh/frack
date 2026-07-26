// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

//! Setlists: ordered programmes for a rehearsal or concert, stored as plain
//! `.txt` files under `setlists_dir` (see [`crate::config`]). One score per
//! line, in order; paths are relative to `root_dir`. A line starting with
//! `#` is a comment or section header. A `#page=` fragment at the end of an
//! entry pins it to part of the PDF (1-based, inclusive):
//!
//! ```text
//! # Autumn concert — 2025-11-14
//! Wagner/Ride of the Valkyries.pdf
//! Combined/Trombones.pdf#page=5-6
//! # Encore
//! Etudes/Arban.pdf#page=12-
//! ```
//!
//! This module only parses and resolves against a root directory (existence
//! only); it never opens the PDFs, so it stays pure and cheap to unit-test.

use std::path::{Path, PathBuf};

/// A page range from a `#page=` fragment, 1-based and inclusive. `None` on a
/// side means an open end (`3-` = from 3 to the end, `-4` = start to 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageRange {
    /// Well-formed range. At least one bound is `Some`; if both are `Some`,
    /// `lo <= hi`.
    Valid { lo: Option<usize>, hi: Option<usize> },
    /// A `#page=` was present but could not be parsed (e.g. `#page=abc`,
    /// `#page=5-3`, `#page=0`). Treated like a broken entry when opened.
    Invalid,
}

/// One score reference: a path relative to `root_dir` and an optional range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub rel: String,
    pub range: Option<PageRange>,
}

/// A parsed line of a setlist file. Blank lines are dropped while parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    /// A `#` line: a title or a section header such as *Encore*.
    Comment(String),
    /// A score reference.
    Entry(Entry),
}

/// A parsed setlist: its display name (file name without `.txt`) and its
/// lines in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setlist {
    pub name: String,
    pub lines: Vec<Line>,
}

/// A setlist entry resolved against a root directory. `range` is carried
/// through unchanged; whether it fits the PDF's page count is only known once
/// the file is opened, so that check lives in the viewer, not here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub rel: String,
    pub abs: PathBuf,
    pub exists: bool,
    pub range: Option<PageRange>,
}

/// The display name of a setlist file: its file name without the extension
/// (`2025-11-14-bbn.txt` -> `2025-11-14-bbn`, `example.txt` -> `example`).
pub fn display_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Parses setlist text. `name` is the display name (see [`display_name`]).
pub fn parse(text: &str, name: &str) -> Setlist {
    let lines = text.lines().filter_map(parse_line).collect();
    Setlist {
        name: name.to_string(),
        lines,
    }
}

/// Reads and parses a setlist file.
pub fn load(path: &Path) -> std::io::Result<Setlist> {
    let text = std::fs::read_to_string(path)?;
    Ok(parse(&text, &display_name(path)))
}

/// A single raw line -> a [`Line`], or `None` for blank/whitespace-only lines.
fn parse_line(raw: &str) -> Option<Line> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(rest) = line.strip_prefix('#') {
        return Some(Line::Comment(rest.trim().to_string()));
    }
    Some(Line::Entry(parse_entry(line)))
}

/// Splits a trailing `#page=…` fragment off the path and parses it.
fn parse_entry(line: &str) -> Entry {
    match line.rfind("#page=") {
        Some(idx) => {
            let rel = line[..idx].trim_end().to_string();
            let spec = &line[idx + "#page=".len()..];
            Entry {
                rel,
                range: Some(parse_range(spec)),
            }
        }
        None => Entry {
            rel: line.to_string(),
            range: None,
        },
    }
}

/// Parses a `#page=` range spec. Grammar (1-based, inclusive):
/// `N` (single page), `N-M`, `N-` (to the end), `-M` (from the start).
/// `,` is reserved for multiple ranges (not yet supported) -> [`PageRange::Invalid`].
fn parse_range(spec: &str) -> PageRange {
    let spec = spec.trim();
    // Multiple comma-separated ranges are a future extension.
    if spec.contains(',') {
        return PageRange::Invalid;
    }
    // A single page `N` behaves like `N-N`.
    let (a, b) = spec.split_once('-').unwrap_or((spec, spec));
    let (lo, hi) = match (parse_side(a), parse_side(b)) {
        (Some(lo), Some(hi)) => (lo, hi),
        _ => return PageRange::Invalid,
    };
    // "-" or "" has no bound at all.
    if lo.is_none() && hi.is_none() {
        return PageRange::Invalid;
    }
    // A closed range must not be inverted.
    if let (Some(l), Some(h)) = (lo, hi)
        && l > h
    {
        return PageRange::Invalid;
    }
    PageRange::Valid { lo, hi }
}

/// One side of a range: empty = open end (`Some(None)`); a valid 1-based page
/// number = `Some(Some(n))`; anything else (non-numeric, `0`) = `None`.
fn parse_side(s: &str) -> Option<Option<usize>> {
    if s.is_empty() {
        return Some(None);
    }
    match s.parse::<usize>() {
        Ok(n) if n >= 1 => Some(Some(n)),
        _ => None,
    }
}

impl Setlist {
    /// The score references in order (comments skipped).
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.lines.iter().filter_map(|l| match l {
            Line::Entry(e) => Some(e),
            Line::Comment(_) => None,
        })
    }

    /// Resolves every entry against `root`, checking existence only.
    pub fn resolve(&self, root: &Path) -> Vec<Resolved> {
        self.entries()
            .map(|e| {
                let abs = root.join(&e.rel);
                Resolved {
                    rel: e.rel.clone(),
                    exists: abs.exists(),
                    abs,
                    range: e.range,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(lo: Option<usize>, hi: Option<usize>) -> Option<PageRange> {
        Some(PageRange::Valid { lo, hi })
    }

    fn entry(line: &str) -> Entry {
        match parse_line(line) {
            Some(Line::Entry(e)) => e,
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[test]
    fn plain_path_has_no_range() {
        assert_eq!(
            entry("foo.pdf"),
            Entry {
                rel: "foo.pdf".into(),
                range: None
            }
        );
    }

    #[test]
    fn path_with_spaces_is_kept() {
        assert_eq!(entry("Bach/Air auf der G.pdf").rel, "Bach/Air auf der G.pdf");
    }

    #[test]
    fn single_page_is_a_one_page_range() {
        assert_eq!(entry("x.pdf#page=3").range, valid(Some(3), Some(3)));
    }

    #[test]
    fn closed_range() {
        assert_eq!(entry("x.pdf#page=3-5").range, valid(Some(3), Some(5)));
    }

    #[test]
    fn open_ended_ranges() {
        assert_eq!(entry("x.pdf#page=3-").range, valid(Some(3), None));
        assert_eq!(entry("x.pdf#page=-4").range, valid(None, Some(4)));
    }

    #[test]
    fn range_keeps_the_path() {
        let e = entry("Combined/Trombones.pdf#page=5-6");
        assert_eq!(e.rel, "Combined/Trombones.pdf");
        assert_eq!(e.range, valid(Some(5), Some(6)));
    }

    #[test]
    fn malformed_ranges_are_invalid_but_keep_the_path() {
        for spec in ["abc", "5-3", "0", "-", "", "1-2,5-6", "1.5"] {
            let e = entry(&format!("x.pdf#page={spec}"));
            assert_eq!(e.rel, "x.pdf", "rel for #page={spec}");
            assert_eq!(
                e.range,
                Some(PageRange::Invalid),
                "range for #page={spec}"
            );
        }
    }

    #[test]
    fn comments_and_sections() {
        assert_eq!(parse_line("# Herbstkonzert"), Some(Line::Comment("Herbstkonzert".into())));
        assert_eq!(parse_line("#Zugabe"), Some(Line::Comment("Zugabe".into())));
    }

    #[test]
    fn blank_lines_are_dropped() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   "), None);
        assert_eq!(parse_line("\t"), None);
    }

    #[test]
    fn lines_are_trimmed() {
        assert_eq!(entry("  x.pdf  ").rel, "x.pdf");
    }

    #[test]
    fn entries_skip_comments_and_keep_order() {
        let sl = parse(
            "# Title\na.pdf\nb.pdf#page=2\n\n# Encore\nc.pdf",
            "demo",
        );
        let rels: Vec<&str> = sl.entries().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels, ["a.pdf", "b.pdf", "c.pdf"]);
        // Comment, Entry, Entry, (blank dropped), Comment, Entry.
        assert_eq!(sl.lines.len(), 5);
        assert_eq!(sl.lines[0], Line::Comment("Title".into()));
        assert_eq!(sl.lines[3], Line::Comment("Encore".into()));
    }

    #[test]
    fn display_name_strips_extension() {
        assert_eq!(display_name(Path::new("example.txt")), "example");
        assert_eq!(
            display_name(Path::new("/x/2025-11-14-autumn-concert.txt")),
            "2025-11-14-autumn-concert"
        );
    }

    #[test]
    fn resolve_marks_missing_and_joins_root() {
        let dir = std::env::temp_dir().join(format!("frack-setlist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("here.pdf"), b"%PDF-1.5\n").unwrap();

        let sl = parse("here.pdf\nmissing.pdf#page=2", "demo");
        let r = sl.resolve(&dir);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].rel, "here.pdf");
        assert!(r[0].exists);
        assert_eq!(r[0].abs, dir.join("here.pdf"));
        assert_eq!(r[1].rel, "missing.pdf");
        assert!(!r[1].exists);
        assert_eq!(r[1].range, valid(Some(2), Some(2)));

        std::fs::remove_dir_all(&dir).ok();
    }
}
