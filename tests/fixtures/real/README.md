# Real GWZ Fixture Snapshot

This directory is a permanent, checked-in test corpus for Tier 1A `explode`.
The files are full Rust source snapshots copied from the local GWZ repositories;
tests must not read the live GWZ workspaces.

Snapshot date: 2026-06-17

Sources:

- `/Users/owebeeone/limbo/glial-dev/gwz-core`
  - commit: `80c41a5f5e209f34926bfcbf4621e9f1f360d355`
- `/Users/owebeeone/limbo/glial-dev/gwz-cli`
  - commit: `f47a4425122fbbea30f3a5ab31aed39d706af828`

Fixture policy:

- Keep whole `.rs` files, not excerpts.
- Keep this directory under version control.
- Treat the snapshot as test data; do not edit copied source files by hand.
- To refresh intentionally, run `scripts/refresh-fixtures.sh` and update this
  README plus the exact fixture path assertion in `tests/explode.rs`.

The copied GWZ sources are GPL-2.0-only, so `rust-split` is licensed
`GPL-2.0-only`.
