// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct Entry {
    /// Absolute path (for symlinks: the path of the link, not its target,
    /// so that setlist folders show up as entries of their own).
    pub path: PathBuf,
    /// Path relative to the root directory, for display and search.
    pub rel: String,
}

/// Searches the root directory recursively for PDF files. Symlinks are
/// followed so that setlists (folders of symlinks) work; loops and read
/// errors are skipped.
pub fn scan(root: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext.eq_ignore_ascii_case("pdf"))
                .unwrap_or(false)
        })
        .map(|e| {
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap_or(e.path())
                .to_string_lossy()
                .into_owned();
            Entry {
                path: e.path().to_path_buf(),
                rel,
            }
        })
        .collect();
    entries.sort_by_key(|e| e.rel.to_lowercase());
    entries
}

/// Simple search: all whitespace-separated terms must occur in the
/// relative path (case-insensitively).
pub fn matches(entry: &Entry, query: &str) -> bool {
    let hay = entry.rel.to_lowercase();
    query
        .split_whitespace()
        .all(|term| hay.contains(&term.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rel: &str) -> Entry {
        Entry {
            path: PathBuf::from("/x").join(rel),
            rel: rel.to_string(),
        }
    }

    #[test]
    fn matches_all_terms_case_insensitive() {
        let e = entry("Bach/Goldberg Variationen.pdf");
        assert!(matches(&e, "bach gold"));
        assert!(matches(&e, ""));
        assert!(!matches(&e, "bach mozart"));
    }
}
