# 03 — Hierarchical trees and path resolution

OMP uses Git's **nested-tree** model: each directory is its own tree object, referenced by its parent. Paths with slashes are not flat names — they're walks through a chain of tree objects.

## The mental model

A commit has exactly one `tree` field: the **root tree**. The root tree lists everything at the top level of the repo:

```
# root tree
blob      ef56...  omp.toml
tree      1234...  schemas
tree      5678...  media-example1
tree      9abc...  reports
```

Each `tree` entry references another tree object. Looking inside `media-example1`:

```
# tree media-example1
manifest  aaaa...  intro.mp3
manifest  bbbb...  video.mp4
tree      cccc...  transcripts
```

And inside `media-example1/transcripts`:

```
# tree media-example1/transcripts
manifest  dddd...  intro.md
manifest  eeee...  video.md
```

To resolve `media-example1/transcripts/intro.md`:

1. Start at the commit's root tree.
2. Look up `media-example1` → get child tree hash (`5678...`).
3. Load that tree. Look up `transcripts` → get child tree hash (`cccc...`).
4. Load that tree. Look up `intro.md` → get manifest hash (`dddd...`).
5. Load the manifest. Read `source_hash` to get the blob of the actual file bytes.

## Why nested instead of flat

A flat tree would let us write:

```
# hypothetical flat tree
manifest  aaaa...  media-example1/intro.mp3
manifest  bbbb...  media-example1/video.mp4
manifest  dddd...  media-example1/transcripts/intro.md
```

We don't do this. Reasons:

1. **Subtree deduplication for free.** Rename a directory from `drafts/` to `published/` and only the *parent* trees change; the renamed subtree keeps its hash. Flat paths force every entry to be rewritten on any ancestor rename.
2. **Partial loading.** An agent browsing `media-example1/` doesn't need to load the entire repo tree — just the root tree and the `media-example1` subtree. Flat trees have no such natural boundary.
3. **Recursive operations are natural.** Listing a subdirectory (`omp ls media-example1/`) is literally "load that subtree and return its entries." With flat trees you'd filter by prefix.
4. **It's Git's model.** Tooling expectations, mental models, and design conversations all map onto existing Git intuition.

## Path resolution — `paths.rs`

The `paths` module (`crates/omp-core/src/paths.rs`) centralizes path-walking logic. Two primary operations:

### `get_at(path: &str, root: &Hash) -> Result<Option<(Mode, Hash)>>`

Walk `path` from `root`, returning the final entry's `(mode, hash)` or `None` if any segment is missing.

Pseudocode:

```rust
fn get_at(path: &str, root: &Hash) -> Option<(Mode, Hash)> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut current = load_tree(root);
    for part in &parts[..parts.len() - 1] {
        let entry = current.lookup(part)?;
        if entry.mode != Mode::Tree { return None; }
        current = load_tree(&entry.hash);
    }
    let last = current.lookup(parts.last()?)?;
    Some((last.mode, last.hash))
}
```

### `put_at(path: &str, new_entry: (Mode, Hash), root: &Hash) -> Result<Hash>`

Add or replace an entry at `path`, returning the new root tree hash. Creates intermediate trees as needed. Bottom-up reconstruction:

Pseudocode:

```rust
fn put_at(path: &str, entry: (Mode, Hash), root: Option<&Hash>) -> Hash {
    let parts: Vec<&str> = path.split('/').collect();
    put_inner(&parts, entry, root)
}

fn put_inner(parts: &[&str], entry: (Mode, Hash), tree: Option<&Hash>) -> Hash {
    let mut current = tree.map_or_else(Tree::empty, load_tree);
    if parts.len() == 1 {
        current.set(parts[0], entry);
        return store(&current);
    }
    let (head, tail) = (parts[0], &parts[1..]);
    let child = current.lookup(head)
        .filter(|e| e.mode == Mode::Tree)
        .map(|e| e.hash);
    let new_child = put_inner(tail, entry, child.as_ref());
    current.set(head, (Mode::Tree, new_child));
    store(&current)
}
```

Every operation that edits the tree (`add`, `patch-fields`, `delete`) boils down to a `put_at` call at the appropriate path. The result is always a new root tree hash, which becomes part of the next commit.

### `delete_at(path, root_tree_hash) -> new_root_tree_hash`

Remove an entry. Similar recursion; on the way back up, any parent tree that becomes empty as a result is itself removed from its own parent. This matches Git's behavior — empty directories don't exist in tree objects — and keeps the root tree canonical (one hash per set of leaf entries, regardless of the order operations arrived in).

### `walk(root_tree_hash) -> Iterator[(path, mode, hash)]`

Depth-first iteration of every leaf entry in the tree, visiting each directory's entries in the tree object's canonical order (lexicographic by name — see `02-object-model.md`). Walk output is therefore deterministic and stable across runs. Used by commands like `omp log --files` or `omp ls --recursive`.

## Listing a directory

`omp ls <path>` and `GET /tree/<path>` both resolve the path with `get_at`, require the result's mode to be `tree`, load that tree, and return its entries. With `?at=<commit>`, the root tree is the commit's root tree; otherwise it's HEAD's.

## Edge cases

- **Empty path / root listing:** `get_at("", root)` returns `("tree", root)`. `omp ls` with no args lists the root tree.
- **Trailing slashes:** normalized away before splitting.
- **Names containing slashes:** forbidden. A single path component is whatever appears between slashes; OMP rejects creating an entry whose name contains `/`.
- **Case sensitivity:** case-sensitive, like Git. A repo with both `Foo.md` and `foo.md` has two distinct entries. On case-insensitive filesystems (macOS default, Windows), checkout will fail loudly if such a collision exists.

## Maximum depth

No hard limit in v1. The recursive `put_inner` walk would blow the stack well past any realistic depth; a repo with 500-level-deep paths is pathological but not broken. If deep paths ever become common, `put_inner` converts to an explicit stack without changing the interface.

## Why `paths.rs` is its own module

Path resolution is the only place in OMP where tree-walking logic lives. Keeping it in one small module makes it easy to test exhaustively (nested fixtures, missing segments, deep puts) and easy to optimize later (caching loaded trees in-process, parallel fetches from a remote store). All the higher-level API functions in `omp_core::api` call into `paths` rather than re-implementing the walk.
