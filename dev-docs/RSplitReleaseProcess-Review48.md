# Review 48: `RSplitReleaseProcess.md`

**Document reviewed:** `dev-docs/RSplitReleaseProcess.md`
**Review date:** 2026-06-17
**Supersedes:** `RSplitReleaseProcess-Review25.md` (its issues are now closed — see below)
**Reviewer scope:** Accuracy vs cargo-dist 0.31.0, smoke-test correctness, and operational completeness.
**Verification method:** Claims were checked against the repo's vendored cargo-dist 0.31.0
reference installers (`installer/install.sh`, `installer/install.ps1` — both uv 0.11.21 samples)
and `Cargo.toml`, not from memory of cargo-dist behavior.

## Summary

The doc has absorbed essentially all of Review25 and is now a tight, accurate runbook.
It is **Approve with changes**. There is exactly **one blocker** before the smoke
tests are copied into CI — and it is the same *class* of bug Review25 flagged (a
verification step that silently passes), just moved from the Unix path bug to the
**Windows assertions**.

## Prior round (Review25) — closed

Spot-checked each item; all addressed in the current doc:

| Review25 item | Status |
|---|---|
| 1. Installer path bug (`*_INSTALL_DIR` → `bin/`) | Fixed — switched to `RUST_SPLIT_UNMANAGED_INSTALL` (flat); Unix path now correct (verified vs `install.sh:1242-1244,1279-1286`) |
| 2. `dist init` example didn't match steps; pin version | Fixed (lines 44-55, `--version 0.31.0`) |
| 3. `[package.metadata.dist] dist = true` | Fixed (lines 34-35) |
| 4. Git repo requirement | Fixed (line 11, line 241) |
| 5. Checksum artifacts underspecified | Fixed (lines 107-108) |
| 6-9. Typo / cross-link / changelog / `dist generate` | Fixed (lines 6-7, 23, 74-75, 113-114) |
| Gaps (PR CI, `[profile.dist]`, `<owner>`, first-release ordering, `*_DOWNLOAD_URL`) | All now present |

## Critical

### C1. Windows smoke-test assertions are no-ops — the job passes even when the binary is wrong

The Unix script asserts correctly: under `set -eu`, `grep` (lines 171-172) and
`test -f` (line 178) return non-zero on failure and abort the script.

The Windows script does **not** assert. `Select-String` with no match returns
nothing and is **not** a terminating error, and `Test-Path` returns `$false`
without throwing — so `$ErrorActionPreference = "Stop"` does not catch either.
Lines 207, 208, and 214 therefore only print; the job exits 0 even if `--version`
is wrong, `--help` lacks `explode`, or `manifest.toml` was never produced.

This is the one thing that makes the Windows verification unsafe to automate —
a green CI run would not prove the Windows install path works.

**Fix:** make each check throw.

```powershell
if (-not (& $exe --version | Select-String "rust-split $version")) { throw "version mismatch" }
if (-not (& $exe --help    | Select-String "explode"))            { throw "help missing 'explode'" }
# ...
if (-not (Test-Path (Join-Path $out "manifest.toml"))) { throw "manifest.toml not produced" }
```

## Medium

### M1. The `RUST_SPLIT_INSTALL_DIR` note contradicts cargo-dist 0.31.0

Lines 146-149 state `RUST_SPLIT_INSTALL_DIR` "uses cargo-dist's managed layout and
installs into an extra `bin/` directory below the requested prefix." In 0.31.0 the
app-specific `*_INSTALL_DIR` override forces a **flat** layout — the binary lands
directly in the prefix, *not* under `bin/` (verifiable in the vendored reference:
`install.sh:1236-1238` sets `_install_layout="flat"`; `:1279-1286` installs to
`$_force_install_dir`). The `bin/` (cargo-home) layout only kicks in when the prefix
equals the Cargo home dir (`install.sh:1249-1257`).

This is harmless to the smoke tests (they use `UNMANAGED_INSTALL`), but it's an
inaccurate note in a runbook — and notably it's the *carried-over rationale* from
Review25's original "critical" claim, which was version-stale. Either correct the
note (flat, not `bin/`, except the cargo-home special case) or drop it, since
`*_INSTALL_DIR` isn't used in verification.

### M2. The `RUST_SPLIT_*` env-var names are predictions — gate them on the first generated installer

Release infra isn't generated yet (no `dist-workspace.toml`; `Cargo.toml` still
lacks `[package.metadata.dist]`/`[profile.dist]`). So `RUST_SPLIT_UNMANAGED_INSTALL`,
`RUST_SPLIT_NO_MODIFY_PATH`, `RUST_SPLIT_DOWNLOAD_URL` are *predicted* names. They
follow cargo-dist's derivation correctly — the uv samples confirm the pattern
(`uv` → `UV_UNMANAGED_INSTALL`, etc.; `install.sh:61`), and `rust-split` uppercases
to `RUST_SPLIT_` — so they're very likely right. But a wrong prefix silently breaks
*every* smoke test. Add one explicit gate to the Release Checklist: on the first
release, confirm the generated `rust-split-installer.sh` actually reads these names
(`grep RUST_SPLIT_ rust-split-installer.sh`) before trusting the verification.

### M3. The process doc never says the committed `installer/*` are uv samples

`RSplitRelease.md:36-38` documents that `installer/install.{sh,ps1}` are uv
reference samples. The process doc — which is what a release operator follows —
doesn't. Someone verifying a release could find `installer/install.sh` in the tree,
run it, and install **uv**. One sentence near Installer Verification closes the
footgun: "the checked-in `installer/` scripts are uv reference samples; the real
installer is the generated `rust-split-installer.sh` release asset."

## Minor

- **m1. Version source of truth.** The version is hand-entered in three places
  (`Cargo.toml`, the `v${version}` tag, `RUST_SPLIT_VERSION`). cargo-dist makes the
  **tag** the trigger and errors if it disagrees with `Cargo.toml`. Failure Handling
  (line 260) would be sharper noting that cargo-dist enforces tag == `Cargo.toml`
  version, so a mismatch fails the release build rather than shipping.
- **m2. `--version` format assumption.** The asserts require the literal
  `rust-split <version>`. That's clap's default (bin name + version), but worth a
  one-line "ensure `--version` prints `rust-split <v>`" so a future `clap` tweak
  doesn't silently break the grep.
- **m3.** Unix asserts use bare `grep`; `grep -q` drops the matched-line noise from
  CI logs. Cosmetic.
- **m4. No rollback note.** Failure Handling covers install-time failures but not
  "we shipped a broken release." A line on deleting the tag+release and re-tagging
  (installers are version-pinned, so a re-tag at the same version needs the old
  assets removed) would complete it. Optional.
- **m5. Archive extension.** Expected assets list `.tar.xz` (the cargo-dist default);
  the vendored uv sample uses `.tar.gz` (uv overrode it). The doc already hedges
  (lines 113-114) — one clause ("the uv sample uses `.tar.gz`; our default is
  `.tar.xz`") preempts the cross-check confusion. Optional.

## Strengths

- **Prior round fully closed**, including the correct call to use
  `UNMANAGED_INSTALL` (flat, predictable) over `INSTALL_DIR`; the Unix path is
  correct against the 0.31.0 reference.
- **Release gate on the published install path**, not local `dist build` — the right
  gate, and the post-release CI job tests exactly what users run.
- **Asset list, target matrix, and failure modes** match cargo-dist 0.31.0 defaults
  and `RSplitRelease.md`.

## Recommended actions

1. **Must fix (CI blocker):** make the Windows asserts throw (C1).
2. **Should fix:** correct or drop the `RUST_SPLIT_INSTALL_DIR` note (M1); add the
   "verify generated env-var names on first release" gate (M2); note the committed
   `installer/*` are uv samples (M3).
3. **Nice to have:** tag/version enforcement note (m1), `--version` format and
   `grep -q` (m2/m3), rollback and archive-extension clauses (m4/m5).

## Conclusion

The runbook is close to ready. The single blocker is C1: until the Windows asserts
throw, the Windows verification can report green without proving anything — the same
"silent pass" risk Review25 flagged on the Unix side. Fix C1 before wiring the smoke
tests into the post-release CI job; M1-M3 are accuracy and footgun fixes that cost a
few lines each. Nothing here requires re-architecting the process.
