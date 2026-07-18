// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

#[derive(Clone, Debug)]
pub struct Entry {
    /// Absolute path (for symlinks: the path of the link, not its target,
    /// so that setlist folders show up as entries of their own).
    pub path: PathBuf,
    /// Path relative to the root directory, for display and search.
    pub rel: String,
}

/// Whether a walked entry is excluded from the library. Currently that is
/// everything hidden (a dot-prefixed name), which prunes VCS/sync
/// internals such as `.git` (git-annex resolves scores to symlinks into
/// `.git/annex/objects`) and `.stversions` (Syncthing) — those would
/// otherwise show up as content-addressed blobs with no real name. The
/// root itself is never excluded, so a hidden `root_dir` still scans.
///
/// This is the single gate for exclusions: a user-configurable ignore
/// list would extend it here.
fn is_ignored(entry: &DirEntry) -> bool {
    entry.depth() > 0
        && entry
            .file_name()
            .to_string_lossy()
            .starts_with('.')
}

/// Searches the root directory recursively for PDF files. Symlinks are
/// followed so that setlists (folders of symlinks) work; loops and read
/// errors are skipped. Hidden files and directories are ignored (see
/// [`is_ignored`]).
pub fn scan(root: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
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

    #[cfg(unix)]
    #[test]
    fn scan_skips_hidden_but_follows_working_tree_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!("frack-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let objects = dir.join(".git/annex/objects");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::create_dir_all(dir.join("Brahms")).unwrap();
        std::fs::create_dir_all(dir.join(".stversions")).unwrap();

        // git-annex style: the score is a symlink into .git/annex/objects.
        std::fs::write(objects.join("HASH.pdf"), b"%PDF-1.5\n").unwrap();
        symlink("../.git/annex/objects/HASH.pdf", dir.join("Brahms/foo.pdf")).unwrap();
        // Must not surface: the annex blob, a Syncthing version, a dotfile.
        std::fs::write(dir.join(".stversions/old.pdf"), b"%PDF-1.5\n").unwrap();
        std::fs::write(dir.join(".hidden.pdf"), b"%PDF-1.5\n").unwrap();

        let rels: Vec<String> = scan(&dir).into_iter().map(|e| e.rel).collect();
        assert_eq!(rels, vec!["Brahms/foo.pdf".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_scans_a_hidden_root_dir() {
        let dir = std::env::temp_dir().join(format!("frack-scan-root-{}", std::process::id()));
        let root = dir.join(".scores");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("foo.pdf"), b"%PDF-1.5\n").unwrap();

        let rels: Vec<String> = scan(&root).into_iter().map(|e| e.rel).collect();
        assert_eq!(rels, vec!["foo.pdf".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
