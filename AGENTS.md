# AGENTS

These rules apply to work in this repository.

- Work TDD-first for behavior changes: failing test, implementation, green tests, then refactor.
- `rust-split` is a mechanical helper. It decides how to split Rust source after a project rule or human/agent judgment decides a split is warranted; it does not decide when files must be split.
- Preserve declaration attributes, visibility, comments, and owning scopes when changing split behavior. Conditional gates must travel with the declaration or module boundary they guard.
- In C-style languages, every control-flow body must use braces, including single-statement and empty bodies.
- In Rust, conditional compilation must have an explicit enclosing boundary such as `cfg_if::cfg_if!` or a platform module. Do not put `#[cfg(...)]` or conditional `cfg_attr` directly on individual imports or other unbraced declarations.
- Keep split changes reviewable. Do not combine relocation, formatting, renaming, and feature behavior in one diff unless the task explicitly requires it.
- Never publish to crates.io from a local checkout. Real crate publishing belongs only in the GitHub Actions release workflow triggered by `release.published`.
- Local crates.io checks must be non-mutating: use `cargo package --list` and `cargo publish --dry-run`, never `cargo publish`.
