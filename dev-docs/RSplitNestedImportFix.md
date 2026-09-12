# rust-split 0.1.4 — nested-module import paths and include! paths (fix brief)

Status: DRAFT brief for an implementing agent. Nothing here is done yet.
Written 2026-09-12 from the empirical run recorded in
`garnets-wz/dev-docs/gwz-go-to-market/research/rust-split-landscape.md`
(private repo; the relevant numbers are copied below).

## Why

`rust-split split --module --max-loc 500` on two real nested test modules
(`gwz-core/src/operation/commit_log/tests.rs`, 3015 LOC, and
`gwz-cli/src/tests/g12.rs`, 2077 LOC) leaves the crate with 178 and 94
`cargo check --all-targets` errors respectively. One systematic cause accounts
for almost all of them:

```
error[E0432]: unresolved import `super::merge`
  --> src/operation/commit_log/tests/l_rng_4_…_degrades_root.rs:10:12
   |
10 | use super::merge::{CommitLogMergedEvent, stream_request_histories};
   |            ^^^^^ could not find `merge` in `super`
```

69 × E0432 of that shape on the first file, plus 109 name-not-found errors
(E0425/E0422) that are the cascade from those failed imports. The second file:
42 unresolved imports, 45 cascade, and 7 of this shape:

```
error: couldn't read `gwz-cli/src/tests/g12/../../docs/commands/clone.md`: No such file or directory
17 |     include_str!("../../docs/commands/clone.md");
```

On a crate-level module without these constructs (`protocol/generated.rs`,
6490 LOC) the split compiles clean with 0 errors, so the machinery is right;
only the relative-path constructs are wrong. `explode` was byte-identical on
all three files.

## Root cause (pointers, verify before editing)

- `src/reassemble.rs` `select_imports` (~line 602) copies the header's `use`
  chunks verbatim into each part that references a name they provide.
- `part_file` (~line 490) assembles `{imports}\n{sibling glob}\n{body}`; the
  glob is `use super::*;` for `Topology::Mod` (~line 142) and `use crate::*;`
  for the binary-root topology.
- A part written under `<stem>/` is one module level deeper than the root it
  came from, so a copied `use super::X` now names the root (the old file) and
  `use self::X` names the part itself. Nothing rewrites them. The root file
  keeps the header unchanged, which is correct — the root is the old module.
- `include_str!` / `include_bytes!` / `include!` resolve their string argument
  relative to the containing file; a moved body is one directory deeper and
  nothing re-bases the literal.

## Phase 1 — rewrite copied header imports (foundation)

Goal: parts compile against the same names the original file saw.

Steps (TDD-first, per AGENTS.md: failing test → implementation → green → refactor):

1. Fixture + failing test: a nested module file whose header has
   `use super::sibling::Thing;`, `use super::{a, b::c};`, `use self::local::X;`
   and a bare `use super::*;`, with items that reference those names. Split it
   with `--module`. Assert each part carries `use super::super::sibling::Thing;`,
   `use super::super::{a, b::c};`, `use super::local::X;` and
   `use super::super::*;`; assert the root's header is byte-unchanged; assert the
   tool's own added sibling glob `use super::*;` is still present exactly once
   per part and is not confused with the header's rewritten glob.
2. Implement the rewrite at the copy point (`select_imports` or `part_file`),
   keyed on topology: for `Topology::Mod`, a copied import path starting with
   `super::` gets one more `super::`, one starting with `self::` becomes
   `super::`; for the binary/crate-root topology, `self::` becomes `crate::`
   (`super::` at a crate root is already invalid; leave it). Only the leading
   segment changes. `crate::` and external paths are untouched. Re-exports
   (`pub use …`) are not copied to parts (they stay at the root verbatim) and
   must not be touched.
3. Extracted inline modules (`mod name { … }` → own file) already handle their
   own `use super::*`; add a test that an inline module body containing
   `use super::helper;` still resolves after extraction (it moves one level
   deeper too — confirm current behaviour with a failing test before changing
   anything there, and record the result in this file).

Budget: < 300 LOC including tests.

## Phase 2 — re-base include! paths in moved bodies

Goal: `include_str!("../x")` in a moved item becomes `include_str!("../../x")`.

Steps:

1. Failing test: an item containing `include_str!("../docs/a.md")`,
   `include_bytes!("fixtures/b.bin")`, and one that must NOT change:
   `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/x"))` and an absolute
   `"/etc/x"`. After `split`, the first two are prefixed with `../` in the part,
   the others are byte-unchanged, and `explode` chunks remain byte-identical to
   the source (explode never rewrites anything).
2. Implement as a text rewrite on the moved chunk only, matching
   `include_str!`, `include_bytes!` and `include!` whose single argument is a
   plain relative string literal (no `/` prefix, not `concat!`/`env!`). Emit one
   warning line on stderr per untouched non-literal argument so the user knows
   where to look.
3. Root file: never rewritten (it did not move).

Budget: < 200 LOC including tests.

## Phase 3 — docs and version

1. README "Limitations": add that a parent's `#[path = "…"]` declaration
   changes where rustc looks for the generated sub-modules, so a `#[path]`-declared
   module must be split with the attribute removed (one line in the parent) or
   the output moved by hand. Also state the new include! re-basing and its
   non-literal exception.
2. `docs/SplitPolicy.md` and `skills/rust-split/SKILL.md`: no change unless the
   loop changes.
3. Version: run `gearu plan --bump patch` (read-only) and report its output. Do
   NOT run `gearu release`, do not tag, do not push, do not publish; the release
   is Gianni's call (RELEASE.md, AGENTS.md).

## Verification (all must pass before reporting done)

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Then the real-file re-run, in a scratch copy only — never in
`/Users/owebeeone/limbo/gwz-dev` itself:

```sh
cp -Rc /Users/owebeeone/limbo/gwz-dev "$SCRATCH/gwz-copy"     # APFS clone, ~50 s
# gwz-core is its own Cargo workspace: check from gwz-copy/gwz-core
cargo run --release -- split "$SCRATCH/gwz-copy/gwz-core/src/operation/commit_log/tests.rs" --module --max-loc 500 --out "$SCRATCH/c2"
# copy $SCRATCH/c2/. over src/operation/commit_log/, then:
(cd "$SCRATCH/gwz-copy/gwz-core" && cargo check -p gwz-core --all-targets 2>&1 | grep -c '^error')
# same for gwz-cli/src/tests/g12.rs (crate name `gwz`, check from gwz-copy root)
```

Record before/after error counts (0.1.3: 178 and 94) in a "Results" section at
the bottom of this file. Target: 0 on the first file; on the second, 0 beyond
whatever include! arguments are non-literal.

## Rules for the implementing agent

- Work on a branch named `nested-import-fix` in `/Users/owebeeone/limbo/rust-split`.
  Commit there. Do not merge, push, tag or publish. No `Co-Authored-By` trailer.
- AGENTS.md applies: TDD-first, braces on every control-flow body, no bare
  `#[cfg]` on imports, split changes reviewable — do not mix formatting or
  renames into the fix commits.
- Do not modify `/Users/owebeeone/limbo/gwz-dev` or anything under it.

## Results

Run 2026-09-12 on branch `nested-import-fix` (worktree
`<scratch>/rust-split-fix`, commits `62748d5..71fb58d`). Cargo.toml version left
at 0.1.3 — `gearu` is not installed on this machine, so `gearu plan --bump patch`
could not be run and no version change was made.

### Gates

```
cargo fmt --all -- --check                                exit 0, no output
cargo test                                                exit 0
    lib            37 passed; 0 failed
    tests/explode  12 passed; 0 failed
    tests/split     6 passed; 0 failed
cargo clippy --all-targets --all-features -- -D warnings  exit 0
```

### Real-file re-run

Same procedure for both columns: reset the member repo, `rust-split split <file>
--module --max-loc 500 --out <tmp>`, copy the output over the source directory,
`cargo check -p <pkg> --all-targets`, count lines matching `^error`. Both crates
compile with 0 errors before the split is applied (control). The counts below
include the trailing `error: could not compile …` summary line, which is why
they are one higher than the 178/94 in the Why section above.

| file | 0.1.3 | this branch |
|---|---|---|
| `gwz-core/src/operation/commit_log/tests.rs` (3015 LOC, 20 output files) | 179 | **0** |
| `gwz-cli/src/tests/g12.rs` (2077 LOC, 18 output files) | 95 | **3** (2 errors + summary) |

Remaining errors on the second file, verbatim:

```
error[E0599]: no associated function or constant named `command` found for struct `globalargs::parser::Cli` in the current scope
   --> gwz-cli/src/tests/g12/clone_local_is_removed_without_an_alias.rs:129:24
error[E0599]: no associated function or constant named `command` found for struct `globalargs::parser::Cli` in the current scope
  --> gwz-cli/src/tests/g12/local_and_clone_help_describe_the_family_surface.rs:20:28
error: could not compile `gwz` (lib test) due to 2 previous errors; 23 warnings emitted
```

Neither is an import-path or an `include!` failure. Both are the **pre-existing
aggressive import-drop policy** (`select_imports`, documented in its own doc
comment and in the README): the header's `use clap::CommandFactory;` is dropped
from a part that reaches the trait only through `Cli::command()` and never names
`CommandFactory`. They were masked in the 0.1.3 run by the unresolved-import
cascade ahead of them. Out of scope for this brief; a separate decision about
whether trait imports should be retained unconditionally.

All 7 `include!` failures on the second file are gone. Neither run printed a
non-literal-argument warning, so every `include*!` in both files had a plain
string-literal argument.

Behaviour, not just compilation: on `gwz-core` the split module's own tests were
run before and after — `cargo test -p gwz-core --lib operation::commit_log::tests`
gives **67 passed / 0 failed on both** the untouched tree and the split tree.
(The full `--lib` run has 119 failures under `workspace_ops::merge::v1_lifecycle`
in this snapshot; they reproduce on the untouched tree and are unrelated.)

### Phase 1 step 3 — extracted inline modules

Confirmed, and the brief's premise was wrong: an extracted inline
`#[cfg(test)] mod tests { use super::helper; … }` does **not** move one module
level deeper. `mod tests { … }` inside `foo.rs` is `foo::tests`; after extraction
the file `foo/tests.rs` is still `foo::tests`. Only the *file* moves down a
directory, which matters for `include!` (Phase 2 handles it by file path depth)
and not for module paths. Current behaviour was already correct; the test
`extracted_inline_mod_keeps_its_own_super_paths` passed unchanged and now pins it,
and `split_module_output_compiles_with_rebased_paths` compiles and runs such a
module with rustc. **Pass, no change needed.**

### Scope added beyond Phases 1–2

Phase 1 as written (copied header imports only) left 26 errors on the first file
and 4 on the second: the same root cause in every other place a relative path
hides — expression and type paths (`super::filter::parse(…)`), function-local
`use super::a::B;`, paths inside macro arguments. A third commit
(`fix: re-anchor the relative paths inside a moved body, not just the header`)
extends the same rule to the moved chunks, token-driven, leading segment only.
That is what takes the first file to 0.

Known not covered: a `pub(super)` restriction on a moved item narrows when the
item moves (it should become `pub(in super::super)`); it is left as it is. No
occurrence in either real file.
