# rust-split

`rust-split` is a parser-based helper for breaking up large Rust source files.
It is meant for the mechanical part of a split: find top-level items, preserve
their attached comments and attributes, group related items, and write a module
layout that stays under a requested LOC ceiling.

It does not decide when a file should be split. Use your project's own rule for
that decision, then run this tool when you want the mechanical carve-out.

## Suggested Split Policy

`rust-split` ships with a suggested policy for teams and agents that do not
already have one. Treat it as operating guidance, not as behavior enforced by
the CLI:

- Around 1,000 LOC, review the file's cohesion and decide whether it should be
  split. This is a soft review trigger, not an automatic split at line 1,001.
- When a split is warranted, target responsibility owners below 500 LOC.
- 500 LOC is a ceiling, not a packing target. Prefer cohesive smaller files over
  filling files toward 499 lines.
- Split earlier when a smaller file becomes a dumping ground for unrelated
  concepts.
- Prefer planning ownership boundaries before implementation so large-file
  churn does not become a separate refactor.

See [SplitPolicy.md](docs/SplitPolicy.md) for the full suggested rule.

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

`split` extracts an inline `#[cfg(test)] mod tests { … }` to its own file
itself, the gate traveling to the root declaration. When you hand-finish from
`explode` chunks instead, that is on you: keep the wrapper's attributes with the
moved module and declare it as a top-level `mod` in the destination file (a
file-module's submodule resolves to a subdir).

One thing the tool won't fix for you: registration blocks
(`#[starlark_module]`, framework macros) need re-wrapping into N blocks and
re-registering by hand.

## Workflow

The tool works in two passes. The first (`explode`) is lossless: concatenating
the generated chunk files in order must reproduce the original file exactly. The
second (`split`) rewrites the module graph, so it intentionally adds module
declarations, re-exports, imports, and some `pub(crate)` visibility.

## Install

Install with Cargo:

```sh
cargo install rust-split
```

Install the latest release on macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/owebeeone/rust-split/releases/latest/download/rust-split-installer.sh | sh
```

Install the latest release on Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/owebeeone/rust-split/releases/latest/download/rust-split-installer.ps1 | iex"
```

The `latest` URLs point at the newest non-prerelease GitHub Release. If you want
a pinned install, replace `latest` with a concrete tag such as `v0.1.1`:

```text
https://github.com/owebeeone/rust-split/releases/download/v0.1.1/rust-split-installer.sh
```

Users who already have Rust can install from source:

```sh
cargo install --git https://github.com/owebeeone/rust-split
```

### Smoke Test Installers

Test the Unix installer without modifying `PATH`:

```sh
tmp="$(mktemp -d)"

curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/owebeeone/rust-split/releases/latest/download/rust-split-installer.sh \
  -o "${tmp}/rust-split-installer.sh"

RUST_SPLIT_UNMANAGED_INSTALL="${tmp}/bin" \
RUST_SPLIT_NO_MODIFY_PATH=1 \
sh "${tmp}/rust-split-installer.sh"

"${tmp}/bin/rust-split" --version
"${tmp}/bin/rust-split" --help
```

Test the Windows installer without modifying `PATH`:

```powershell
$ErrorActionPreference = "Stop"

$tmp = Join-Path $env:TEMP "rust-split-test-$([guid]::NewGuid())"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

$installer = Join-Path $tmp "rust-split-installer.ps1"
Invoke-WebRequest `
  "https://github.com/owebeeone/rust-split/releases/latest/download/rust-split-installer.ps1" `
  -OutFile $installer

$env:RUST_SPLIT_UNMANAGED_INSTALL = Join-Path $tmp "bin"
$env:RUST_SPLIT_NO_MODIFY_PATH = "1"

Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
& $installer

$exe = Join-Path $env:RUST_SPLIT_UNMANAGED_INSTALL "rust-split.exe"
& $exe --version
& $exe --help
```

Release assets are checksummed and have GitHub artifact attestations. The
installers are convenience scripts; users who want stronger verification should
download the release asset, verify the attestation, compare the SHA-256 checksum,
and then install.

## Commands

Build a local development binary:

```sh
cargo build
./target/debug/rust-split --help
```

Or install the current checkout onto your Cargo bin path:

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

`split` treats plain `use`/`extern crate` imports and the file preamble as a
shared header. Three chunk classes never enter clustering:

- **Re-exports** (`pub use ...`, and any visibility-qualified import) are the
  file's API surface and stay at the root verbatim — never dropped, never
  demoted to `pub(crate)`.
- **`mod name;` declarations** bind files relative to the root's directory and
  stay at the root verbatim.
- **Inline modules** (`mod name { ... }`, any size) are already module
  boundaries: each is extracted whole to its own file, with its attributes
  (`#[cfg(test)]` keeps gating the declaration), doc comments, and visibility
  traveling to the root's `mod name;`.

Everything else is clustered by sibling-reference adjacency. The LOC ceiling is
a hard upper bound but **not a packing target**: a transitively related group
larger than half the ceiling is partitioned into roughly equal cohesive parts
(strongest reference edges bond first, so cuts fall on the weakest edges),
leaning toward more, smaller files rather than one file grazing the ceiling.
Unrelated items are left separate rather than packed together just to reduce
file count.

The file preamble (inner `//!` docs and `#![...]` attributes) stays at the root.
Each generated file copies only the imports it references by name; `*` globs and
`as _` trait imports, which expose no name, are kept everywhere. Modules reach
their siblings through the generated root:

- binary roots use `use crate::*`
- nested module splits use `use super::*`

The root re-exports each module with a single glob whose visibility matches the
module's widest item: `pub use` when it has a public item (preserving the crate's
public surface), `pub(crate) use` otherwise, and nothing for a module that
exposes no nameable item (such as a bare `impl`). Moved private items, struct
fields, and inherent impl members may be bumped to `pub(crate)` so sibling
modules can still refer to them.

A plain `foo.rs` file module places its sub-modules in a `foo/` subdir — where
`mod bar;` resolves — so the layout compiles without a manual move; a `foo/mod.rs`
keeps its sub-modules as siblings.

Because a generated part is a module *below* the file it came from, the imports
copied into it are re-anchored: a `use super::…` gains one `super::`, and a
`use self::…` becomes `use super::…` (or `use crate::…` below a crate root).
`crate::`, `::`-rooted and external paths are untouched, and the root file — still
the original module — keeps its header verbatim. For the same reason, a relative
`include!` / `include_str!` / `include_bytes!` path in a body that moved into a
subdirectory gains one `../` per level.

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
- A `#[path = "…"]` declaration in the parent changes where rustc looks for the
  generated sub-modules, so a `#[path]`-declared module must be split with the
  attribute removed (one line in the parent) or the output moved by hand.
- `include!` / `include_str!` / `include_bytes!` paths are re-based only when the
  argument is a single plain string literal holding a relative path. An absolute
  path is left alone, and a computed argument — `concat!(env!("CARGO_MANIFEST_DIR"),
  …)`, a macro, a constant — cannot be re-based mechanically: it is left verbatim
  and reported on stderr with the generated file and line to check.
