# rust-split

`rust-split` is a parser-based helper for breaking up large Rust source files.
It is meant for the mechanical part of a split: find top-level items, preserve
their attached comments and attributes, group related items, and write a module
layout that stays under a requested LOC ceiling.

It does not decide when a file should be split. Use your project's own rule for
that decision, then run this tool when you want the mechanical carve-out.

## For agents

Reach for this instead of hand-editing. Moving items by cut-and-paste is O(n²) in
edits and silently orphans doc-comments and `#[attrs]`; `explode` + `split` is
O(n) and verifiable. (This tool decides *how*, never *when* — your repo's rules
own that.)

Loop:

1. `explode` to a temp dir, then **diff the concatenated chunks against the
   original and confirm it is byte-identical** before trusting the split.
2. Read `manifest.toml` and adjust grouping there — don't re-cluster by hand.
3. `split --out <tempdir>` (never in place first), then copy in deliberately.
4. Let the compiler enumerate the fallout: run the crate's build and fix the
   exact `use` / `pub(crate)` errors it reports. Don't predict visibility by
   reading.
5. Proven when the crate's existing tests stay green on a pure-move diff. **Do
   not run a formatter** — it destroys the pure-move diff; formatting is a
   separate pass.

Two things the tool won't fix for you:

- Keep `#[cfg(test)] mod tests { … }` in the moved file and declare it as a
  top-level `mod` in that file (a file-module's submodule resolves to a subdir).
- Registration blocks (`#[starlark_module]`, framework macros) need re-wrapping
  into N blocks and re-registering by hand.

## Workflow

The tool works in two passes. The first (`explode`) is lossless: concatenating
the generated chunk files in order must reproduce the original file exactly. The
second (`split`) rewrites the module graph, so it intentionally adds module
declarations, re-exports, imports, and some `pub(crate)` visibility.

## Commands

Build a local binary:

```sh
cargo build
./target/debug/rust-split --help
```

Or install it onto your Cargo bin path:

```sh
cargo install --path .
rust-split --help
```

While developing the tool, any `rust-split ...` command below can also be run as
`cargo run -- ...`.

Explode a file into chunks:

```sh
rust-split explode path/to/file.rs --out /tmp/file-chunks
```

This writes:

- `chunk-000.rs`, `chunk-001.rs`, ...
- `manifest.toml`

Split a binary crate root, such as `src/main.rs`:

```sh
rust-split split src/main.rs --max-loc 500 --out /tmp/split-main
```

Split a nested module file, such as `src/workspace_ops/mod.rs`:

```sh
rust-split split src/workspace_ops/mod.rs --module --max-loc 500 --out /tmp/split-workspace-ops
```

Omit `--out` to write in place next to the source file. For real work, prefer
using `--out` first, reviewing the generated layout, then copying the result
into the repo deliberately.

## How It Splits

`explode` parses the file with `syn` and records one manifest row per top-level
item. Each row includes:

- item name and kind
- byte range and line span
- LOC
- sibling identifier references as `adjacency_hint`

`split` treats `use`, `extern crate`, and file preamble chunks as a shared
header. Other items are clustered by sibling-reference adjacency, with the LOC
ceiling treated as a hard upper bound. Unrelated items are left separate rather
than packed together just to reduce file count.

Generated module files over-include the shared header and import siblings through
the generated root:

- binary roots use `use crate::*`
- nested module splits use `use super::*`

Moved private items, struct fields, and inherent impl members may be bumped to
`pub(crate)` so sibling modules can still refer to them.

## Verification

Run the tool's own checks with:

```sh
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

After applying a generated split to another crate, run that crate's normal
verification commands. `rust-split` handles the mechanical move, but the compiler
is still the authority for import paths, visibility, macro edge cases, and public
API preservation.

## Limitations

- `adjacency_hint` is syntactic. It records sibling identifier references, not a
  full semantic call graph, so shadowing and macro expansion can affect grouping.
- Registration macros or framework-specific blocks may need manual treatment.
- Large leaf items cannot be split internally; they are reported as still
  oversized.
