---
name: rust-split
description: Use rust-split to mechanically split large Rust files after a split has been judged warranted. Prefer syntax-aware explode/split workflows over cut-and-paste moves.
---

# Rust Split

Use `rust-split` for the mechanical part of splitting Rust source files. It does not decide whether a file must be split; follow the active repository's rule, task, or human judgment for that decision.

## Suggested split policy

- Around 1,000 LOC, review the file's cohesion. This is a soft trigger, not an automatic split at line 1,001.
- When a split is warranted, target responsibility owners below 500 LOC.
- Treat 500 LOC as a ceiling, not a packing target. Prefer cohesive smaller files over filling files toward 499 lines.
- Split earlier when a smaller file becomes a dumping ground for unrelated concepts.
- Prefer planning ownership boundaries before implementation to avoid churn.

## When to use it

- A Rust file is already judged too large, conceptually crowded, or hard to work in.
- You need to preserve comments, attributes, module declarations, re-exports, imports, and visibility while moving top-level items.
- You want a reviewable split pass separate from formatting and unrelated behavior changes.

Do not turn ordinary feature work into a repository-wide split refactor. Defer the split if another active change owns the same file and the merge cost would dominate the benefit.

## Workflow

1. Run `rust-split --help` and confirm the installed CLI supports the needed mode.
2. Run `rust-split explode path/to/file.rs --out <fresh-dir>`.
3. Verify that concatenating chunks in manifest order reproduces the original file byte for byte.
4. Inspect `manifest.toml` for item boundaries and relationship hints. Treat adjacency as evidence, not architecture.
5. Run `rust-split split path/to/file.rs --max-loc 500 --out <fresh-output>`. For nested module files, use `--module`.
6. Review generated root/module files before copying them into the repository. Check imports, re-exports, visibility widening, extracted inline modules, file-module subdirectories, and conditional scopes.
7. Apply the reviewed output deliberately, then run the affected project's compiler and tests.

Keep formatting as a separate pass. A successful split still needs compilation and behavior checks.

## Guardrails

- Move attributes with their declarations or enclosing module boundaries. Never leave `#[cfg]` behind where it can attach to the next declaration.
- Keep unconditional imports outside conditional sections. Use explicit enclosing conditional boundaries where a project requires them.
- Prefer more cohesive smaller files over packing unrelated items near a line-count ceiling.
- Record any oversized or deferred item explicitly instead of claiming the split is complete.
