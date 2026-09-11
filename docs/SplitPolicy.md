# Suggested Split Policy

`rust-split` is a mechanical Rust source splitter. It does not decide when a
file must be split. This policy is the suggested rule for projects and agents
that want a default.

## Rule

- Around 1,000 LOC, perform a cohesion review.
- Do not split automatically just because a file reached 1,001 lines.
- If the file is still cohesive, document the reason and keep working.
- If a split is warranted, create responsibility owners below 500 LOC.
- Treat 500 LOC as a ceiling, not a packing target.
- Prefer cohesive smaller files over minimizing file count.
- Split earlier when a smaller file becomes a dumping ground for unrelated
  concepts.
- Prefer planning ownership boundaries before implementation to avoid churn.

## How to Choose Boundaries

Split by responsibility, not by line count. Useful boundaries include:

- domain concept
- invariant
- lifecycle phase
- data owner
- dependency direction
- public API surface versus private implementation
- test support versus production behavior

Reference adjacency is useful evidence, but it is not architecture. Review the
generated grouping before applying it.

## How to Use rust-split with This Policy

1. Decide that a split is warranted using the project rule or this suggested
   policy.
2. Run `rust-split explode <file> --out <fresh-dir>`.
3. Verify that the chunks in manifest order reproduce the original file byte for
   byte.
4. Run `rust-split split <file> --max-loc 500 --out <fresh-output-dir>`.
5. Review grouping, module names, imports, re-exports, visibility changes,
   comments, attributes, and conditional scopes before applying the output.
6. Let the compiler identify real import and visibility fallout.
7. Run the affected project's normal tests.

Keep formatting as a separate pass. A split should first be reviewable as a
relocation and module-wiring change.

## Exceptions

Some items are intentionally indivisible or belong near an existing boundary.
When an output file remains above the target, record why it is acceptable or what
later nested split should handle it.
