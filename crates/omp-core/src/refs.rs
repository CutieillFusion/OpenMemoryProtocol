//! Helpers for reading HEAD, the current branch, and commit ancestry.

use crate::commit::Commit;
use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::object::ObjectType;
use crate::store::ObjectStore;

/// What HEAD points at.
#[derive(Clone, Debug)]
pub enum Head {
    /// `ref: refs/heads/<branch>`. The branch may or may not yet exist.
    Branch(String),
    /// Detached — HEAD is a raw commit hash.
    Detached(Hash),
}

pub fn parse_head(raw: &str) -> Result<Head> {
    let s = raw.trim();
    if let Some(name) = s.strip_prefix("ref: ") {
        return Ok(Head::Branch(name.trim().to_string()));
    }
    let h: Hash = s
        .parse()
        .map_err(|e| OmpError::Corrupt(format!("HEAD: {e}")))?;
    Ok(Head::Detached(h))
}

pub fn current_branch(store: &dyn ObjectStore) -> Result<Option<String>> {
    match parse_head(&store.read_head()?)? {
        Head::Branch(name) => Ok(Some(name)),
        Head::Detached(_) => Ok(None),
    }
}

/// Resolve HEAD to a commit hash, if one exists. Returns `Ok(None)` if HEAD
/// points at an empty branch (no commits yet).
pub fn resolve_head(store: &dyn ObjectStore) -> Result<Option<Hash>> {
    let head = parse_head(&store.read_head()?)?;
    match head {
        Head::Detached(h) => Ok(Some(h)),
        Head::Branch(name) => store.read_ref(&name),
    }
}

/// Resolve a user-supplied ref expression into a commit hash.
///
/// Accepted forms:
/// - `"HEAD"` → the current HEAD.
/// - `"HEAD~N"` → N commits back from HEAD.
/// - `"main"` → `refs/heads/main` as a shorthand.
/// - `"refs/heads/main"` → ref path verbatim.
/// - a 64-char hex string → literal commit hash.
pub fn resolve_ref(store: &dyn ObjectStore, expr: &str) -> Result<Hash> {
    let (base, back) = split_ancestor(expr);

    let start = if base == "HEAD" {
        resolve_head(store)?
            .ok_or_else(|| OmpError::NotFound("HEAD has no commits".into()))?
    } else if base.starts_with("refs/") {
        store
            .read_ref(base)?
            .ok_or_else(|| OmpError::NotFound(format!("ref {base}")))?
    } else if let Ok(h) = base.parse::<Hash>() {
        h
    } else {
        // Branch shortname.
        let full = format!("refs/heads/{base}");
        store
            .read_ref(&full)?
            .ok_or_else(|| OmpError::NotFound(format!("branch {base}")))?
    };

    nth_ancestor(store, start, back)
}

fn split_ancestor(expr: &str) -> (&str, usize) {
    if let Some((base, tail)) = expr.split_once('~') {
        let n = tail.parse::<usize>().unwrap_or(1);
        (base, n)
    } else {
        (expr, 0)
    }
}

fn nth_ancestor(store: &dyn ObjectStore, mut cur: Hash, mut n: usize) -> Result<Hash> {
    while n > 0 {
        let (ty, content) = store
            .get(&cur)?
            .ok_or_else(|| OmpError::NotFound(format!("commit {}", cur.hex())))?;
        if ty != ObjectType::Commit.as_str() {
            return Err(OmpError::Corrupt(format!(
                "expected commit at {}",
                cur.hex()
            )));
        }
        let commit = Commit::parse(&content)?;
        cur = commit
            .parents
            .first()
            .copied()
            .ok_or_else(|| OmpError::NotFound("no further ancestors".into()))?;
        n -= 1;
    }
    Ok(cur)
}

/// Walk parents from `head` breadth-first, yielding commits in reverse-
/// chronological order (children before parents). `max` caps how many are
/// returned; `filter_path` optionally restricts to commits that changed the
/// given path (v1 implementation: naive — always true).
pub fn log(
    store: &dyn ObjectStore,
    head: Hash,
    max: usize,
) -> Result<Vec<(Hash, Commit)>> {
    let mut out = Vec::new();
    let mut queue = vec![head];
    let mut seen = std::collections::HashSet::new();
    while let Some(h) = queue.pop() {
        if out.len() >= max {
            break;
        }
        if !seen.insert(h) {
            continue;
        }
        let (ty, content) = match store.get(&h)? {
            Some(v) => v,
            None => break,
        };
        if ty != ObjectType::Commit.as_str() {
            break;
        }
        let commit = Commit::parse(&content)?;
        queue.extend(commit.parents.iter().rev().copied());
        out.push((h, commit));
    }
    Ok(out)
}
