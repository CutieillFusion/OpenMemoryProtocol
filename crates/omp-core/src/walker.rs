//! Working-tree walker. See `01-on-disk-layout.md §What a tree walk sees`.
//!
//! Distinct from `paths::walk`, which walks an *object tree*. This walks the
//! filesystem and classifies each encountered file into a tree-entry mode.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::{OmpError, Result};
use crate::tree::Mode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkEntry {
    /// Slash-separated path relative to the repo root.
    pub repo_path: String,
    /// Absolute path on disk.
    pub fs_path: PathBuf,
    /// How this path should appear in the tree.
    pub mode: Mode,
}

#[derive(Clone, Debug, Default)]
pub struct WalkOptions {
    pub ignore_patterns: Vec<String>,
    pub follow_symlinks: bool,
    /// Paths already tracked in HEAD. Ignore patterns do **not** apply to
    /// these — they are walked regardless (matches Git's behavior).
    pub tracked: HashSet<String>,
}

pub fn walk_repo(repo_root: &Path, opts: &WalkOptions) -> Result<Vec<WalkEntry>> {
    let mut out = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    walk_inner(repo_root, repo_root, opts, &mut visited, &mut out)?;
    // Deterministic order.
    out.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));
    Ok(out)
}

fn walk_inner(
    root: &Path,
    dir: &Path,
    opts: &WalkOptions,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<WalkEntry>,
) -> Result<()> {
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(canonical.clone()) {
        if opts.follow_symlinks {
            return Err(OmpError::InvalidPath(format!(
                "symlink loop at {}",
                dir.display()
            )));
        }
        return Ok(());
    }

    let entries = std::fs::read_dir(dir).map_err(|e| OmpError::io(dir, e))?;
    let mut children: Vec<std::fs::DirEntry> = entries
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| OmpError::io(dir, e))?;
    children.sort_by_key(|d| d.file_name());

    for entry in children {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| OmpError::io(&path, e))?;

        // Relative slash path + classification (owned, so the move of `path`
        // below doesn't borrow-check conflict).
        let mode = {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| OmpError::internal("walker prefix"))?;
            classify_path(rel)
        };
        let rel = path
            .strip_prefix(root)
            .map_err(|_| OmpError::internal("walker prefix"))?
            .to_path_buf();
        let repo_path = to_slash(&rel);

        // Never enter .omp/.
        if repo_path == ".omp" || repo_path.starts_with(".omp/") {
            continue;
        }

        let is_tracked = opts.tracked.contains(&repo_path);
        let is_ignored =
            !is_tracked && ignore_matches(&opts.ignore_patterns, &repo_path, metadata.is_dir());
        if is_ignored {
            continue;
        }

        if metadata.file_type().is_symlink() {
            if !opts.follow_symlinks {
                continue;
            }
            // Follow by delegating to the target.
            let target = std::fs::read_link(&path).map_err(|e| OmpError::io(&path, e))?;
            let resolved = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or(root).join(target)
            };
            let target_meta =
                std::fs::metadata(&resolved).map_err(|e| OmpError::io(&resolved, e))?;
            if target_meta.is_dir() {
                walk_inner(root, &resolved, opts, visited, out)?;
                continue;
            }
            out.push(WalkEntry {
                repo_path,
                fs_path: resolved,
                mode,
            });
            continue;
        }

        if metadata.is_dir() {
            walk_inner(root, &path, opts, visited, out)?;
        } else if metadata.is_file() {
            out.push(WalkEntry {
                repo_path,
                fs_path: path,
                mode,
            });
        }
    }

    Ok(())
}

fn to_slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Per `01-on-disk-layout.md`: anything under `schemas/` or `probes/`, or
/// `omp.toml` at the repo root, is a `blob`. Everything else is a `manifest`.
pub fn classify_path(rel: &Path) -> Mode {
    let s = to_slash(rel);
    if s == "omp.toml" {
        return Mode::Blob;
    }
    if s.starts_with("schemas/") || s.starts_with("probes/") {
        return Mode::Blob;
    }
    Mode::Manifest
}

pub fn ignore_matches(patterns: &[String], path: &str, is_dir: bool) -> bool {
    for p in patterns {
        if one_ignore_matches(p, path, is_dir) {
            return true;
        }
    }
    false
}

fn one_ignore_matches(pattern: &str, path: &str, is_dir: bool) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() || pat.starts_with('#') {
        return false;
    }
    let dir_only = pat.ends_with('/');
    let pat = pat.trim_end_matches('/');

    // Split into segments — gitignore-style: a slash in the middle anchors to
    // the working-tree root; otherwise matches at any depth.
    if pat.contains('/') {
        let anchored = pat.strip_prefix('/').unwrap_or(pat);
        // Match against path or a prefix of path.
        return glob_match(anchored, path)
            && (!dir_only || is_dir || path_descends_from(path, anchored));
    }
    // Unanchored — match any path component.
    for component in path.split('/') {
        if glob_match(pat, component)
            && (!dir_only || is_dir || path_descends_from(path, component))
        {
            return true;
        }
    }
    false
}

fn path_descends_from(path: &str, prefix: &str) -> bool {
    path == prefix
        || path.starts_with(&format!("{prefix}/"))
        || path.split('/').any(|c| c == prefix)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_inner(&p, 0, &t, 0)
}

fn glob_inner(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            let mut i = ti;
            loop {
                if glob_inner(p, pi + 1, t, i) {
                    return true;
                }
                if i == t.len() {
                    return false;
                }
                i += 1;
            }
        }
        '?' => {
            if ti == t.len() {
                return false;
            }
            glob_inner(p, pi + 1, t, ti + 1)
        }
        c => {
            if ti == t.len() || t[ti] != c {
                return false;
            }
            glob_inner(p, pi + 1, t, ti + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(p: &Path, content: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn walks_and_classifies() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join(".omp")).unwrap();
        touch(&root.join(".omp/HEAD"), "x");
        touch(&root.join("omp.toml"), "");
        touch(&root.join("schemas/text/schema.toml"), "x");
        touch(&root.join("probes/file/size.wasm"), "x");
        touch(&root.join("docs/readme.md"), "x");

        let entries = walk_repo(root, &WalkOptions::default()).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.repo_path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "docs/readme.md",
                "omp.toml",
                "probes/file/size.wasm",
                "schemas/text/schema.toml",
            ]
        );
        // Classification.
        let get = |p: &str| entries.iter().find(|e| e.repo_path == p).unwrap().mode;
        assert_eq!(get("omp.toml"), Mode::Blob);
        assert_eq!(get("schemas/text/schema.toml"), Mode::Blob);
        assert_eq!(get("probes/file/size.wasm"), Mode::Blob);
        assert_eq!(get("docs/readme.md"), Mode::Manifest);
    }

    #[test]
    fn ignores_respect_tracked_set() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        touch(&root.join("a.log"), "x");
        touch(&root.join("b.md"), "x");

        let opts = WalkOptions {
            ignore_patterns: vec!["*.log".into()],
            follow_symlinks: false,
            tracked: HashSet::new(),
        };
        let entries = walk_repo(root, &opts).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.repo_path.as_str()).collect();
        assert_eq!(paths, vec!["b.md"]);

        // Now pretend a.log is already tracked; ignore should not drop it.
        let mut tracked = HashSet::new();
        tracked.insert("a.log".to_string());
        let opts = WalkOptions {
            ignore_patterns: vec!["*.log".into()],
            follow_symlinks: false,
            tracked,
        };
        let entries = walk_repo(root, &opts).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.repo_path.as_str()).collect();
        assert_eq!(paths, vec!["a.log", "b.md"]);
    }

    #[test]
    fn directory_ignores() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        touch(&root.join("node_modules/x.js"), "x");
        touch(&root.join("src/main.rs"), "x");

        let opts = WalkOptions {
            ignore_patterns: vec!["node_modules/".into()],
            follow_symlinks: false,
            tracked: HashSet::new(),
        };
        let entries = walk_repo(root, &opts).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.repo_path.as_str()).collect();
        assert_eq!(paths, vec!["src/main.rs"]);
    }

    #[test]
    fn skips_dot_omp() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        touch(&root.join(".omp/objects/ab/cd"), "x");
        touch(&root.join("x.md"), "x");
        let entries = walk_repo(root, &WalkOptions::default()).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.repo_path.as_str()).collect();
        assert_eq!(paths, vec!["x.md"]);
    }
}
