# Review: `RSplitReleaseProcess.md`

**Document reviewed:** `dev-docs/RSplitReleaseProcess.md`  
**Review date:** 2025-06-17  
**Reviewer scope:** Accuracy vs cargo-dist 0.31.0, rust-split crate behavior, and operational completeness.

## Summary

`RSplitReleaseProcess.md` is a focused, actionable release runbook. It complements
`RSplitRelease.md` (design and setup context) with checklists, expected assets,
installer smoke tests, and failure handling. The structure is appropriate for someone
cutting a release without re-reading the full cargo-dist docs.

**Verdict:** Approve with changes. One **critical** fix is required in the
installer smoke tests before they are copied into CI. Several smaller gaps and
wording fixes would improve first-run success.

## Strengths

1. **Right level of detail** — Separates one-time setup, release checklist, asset
   expectations, verification, and failure handling without duplicating the full
   cargo-dist tutorial in `RSplitRelease.md`.

2. **Release gate on published installers** — Correctly states that local
   `dist build` does not prove GitHub Release URLs, asset names, or checksum
   wiring. The proposed post-release CI job testing the public install path matches
   how users actually install.

3. **Smoke test contract** — Install to a temp dir, skip PATH mutation, run
   `--version`, exercise `explode`, check `manifest.toml`. That is a minimal
   but meaningful functional check for this tool.

4. **Environment variables** — `RUST_SPLIT_NO_MODIFY_PATH=1` and
   `RUST_SPLIT_VERSION` for scripting are the right knobs. Documenting
   `RUST_SPLIT_DOWNLOAD_URL` for local dry runs (in the Local Dry Run section)
   is useful.

5. **Expected asset list** — Platform triples and archive naming align with
   default cargo-dist output for the configured target set.

6. **Failure handling** — Common failure modes (404, version mismatch, missing
   platform, PATH side effects) are practical and accurate.

## Critical issues

### 1. Installer smoke tests use wrong binary path with `RUST_SPLIT_INSTALL_DIR`

When `RUST_SPLIT_INSTALL_DIR` is set, cargo-dist generated installers use the
**cargo-home layout**: the binary is installed to `$RUST_SPLIT_INSTALL_DIR/bin/`,
not directly under `$RUST_SPLIT_INSTALL_DIR`.

The Unix script sets `RUST_SPLIT_INSTALL_DIR="${tmp}/bin"` then invokes
`"${tmp}/bin/rust-split"`. With cargo-home layout, the binary lands at
`${tmp}/bin/bin/rust-split`.

The Windows script has the same problem: `$exe` points to
`Join-Path $RUST_SPLIT_INSTALL_DIR "rust-split.exe"` but should include an extra
`\bin` segment.

**Fix (option A — keep `RUST_SPLIT_INSTALL_DIR`, adjust paths):**

```sh
RUST_SPLIT_INSTALL_DIR="${tmp}" \
RUST_SPLIT_NO_MODIFY_PATH=1 \
sh "${tmp}/rust-split-installer.sh"

"${tmp}/bin/rust-split" --version | grep "rust-split ${version}"
```

```powershell
$env:RUST_SPLIT_INSTALL_DIR = $tmp
$exe = Join-Path $env:RUST_SPLIT_INSTALL_DIR "bin\rust-split.exe"
```

**Fix (option B — flat layout via `RUST_SPLIT_UNMANAGED_INSTALL`):**

```sh
RUST_SPLIT_UNMANAGED_INSTALL="${tmp}/bin" \
RUST_SPLIT_NO_MODIFY_PATH=1 \
sh "${tmp}/rust-split-installer.sh"
```

Option B also disables the updater (desirable for smoke tests). Either option is
fine; the doc should state explicitly which layout each env var selects.

## Medium issues

### 2. One-time setup commands do not match numbered steps

Steps 3–4 say to enable shell/PowerShell installers and list five targets, but the
example block only shows:

```sh
cargo install cargo-dist
dist init
dist plan
```

A first-time operator may accept defaults that differ from the intended matrix. Align
the example with the steps, e.g.:

```sh
dist init --yes \
  --installer shell \
  --installer powershell \
  --target aarch64-apple-darwin \
  --target x86_64-apple-darwin \
  --target aarch64-unknown-linux-gnu \
  --target x86_64-unknown-linux-gnu \
  --target x86_64-pc-windows-msvc
```

Also note that `cargo install cargo-dist` may not pin the same version as CI
(`cargo-dist-version` in `dist-workspace.toml`). Prefer the versioned curl
installer or `cargo install cargo-dist --version 0.31.0` for parity with generated
workflows.

### 3. Missing explicit `[package.metadata.dist] dist = true`

Step 1 only mentions `repository`. For a single-crate repo, `dist init` may infer
distribution, but the design doc in `RSplitRelease.md` recommends setting
`[package.metadata.dist] dist = true` explicitly. Add it to the one-time setup
list to avoid ambiguous or failed releases.

### 4. Git repository requirement not stated

`dist build` fails without a git repo when generating `source.tar.gz`. The Local
Dry Run section should mention `git init` + at least one commit (or that CI
always satisfies this) so local pre-tag checks do not fail opaquely.

### 5. Checksum artifacts underspecified

“checksum files” is vague. A complete release also includes:

- Per-artifact `*.sha256` sidecars
- Aggregate `sha256.sum`

Listing these alongside the archives sets clearer expectations when auditing a release
page.

## Minor issues

### 6. Typo

Line 76: “differently” is not a word. Change to **“configured differently”**.

### 7. No cross-link to design doc

`RSplitRelease.md` points here for checklists and smoke tests. This doc should
link back for context (cargo-dist overview, minimal `dist-workspace.toml`, uv
boundary, env var table).

### 8. Changelog is optional but unnamed

Step 3 says “changelog if one exists.” The repo currently has no `CHANGELOG.md`.
Either note that fact or say “README/release notes” until a changelog exists.

### 9. `dist generate` missing from one-time setup

If someone edits `dist-workspace.toml` by hand after init, they need
`dist generate` before `dist plan`. The doc covers this under “If config changes
later” but not in the initial commit step (step 5). Harmless if init always runs
generate; worth one line in step 5: “confirm `dist generate` ran (init does this
automatically).”

## Gaps (optional additions)

| Gap | Suggestion |
|-----|------------|
| No mention of PR CI behavior | One sentence: tag push runs full release; PRs run `dist plan` only by default |
| `[profile.dist]` | Note that `dist init` adds it; required for `dist build` |
| Placeholder `<owner>` | State once that all smoke-test URLs need a real GitHub owner |
| First release chicken-and-egg | Smoke tests require published assets; first release must complete CI before smoke tests pass |
| `RUST_SPLIT_DOWNLOAD_URL` override | Local dry run mentions it; smoke-test section could show overriding base URL for staging |

## Consistency with `RSplitRelease.md`

The two docs split roles well:

| `RSplitRelease.md` | `RSplitReleaseProcess.md` |
|--------------------|---------------------------|
| Why and what (design) | How and when (operations) |
| cargo-dist concepts, uv boundary | Checklists and commands |
| Config examples | Expected release assets |
| Troubleshooting reference | Installer smoke tests + CI verification plan |

No major contradictions except that the design doc’s env var table lists
`RUST_SPLIT_UNMANAGED_INSTALL` while the process doc uses
`RUST_SPLIT_INSTALL_DIR` in smoke tests — both valid, but the layout difference
should be documented in one place to avoid the path bug above.

## Recommended action items

1. **Must fix:** Correct installer smoke-test paths (or switch to
   `RUST_SPLIT_UNMANAGED_INSTALL`) in Unix and Windows sections.
2. **Should fix:** Expand `dist init` example to match installer/target steps; pin
   cargo-dist version.
3. **Should fix:** Add `[package.metadata.dist] dist = true`, git repo note, and
   explicit checksum file names.
4. **Nice to have:** Cross-link to `RSplitRelease.md`, fix “differently” typo,
   note first-release ordering (CI publish before smoke test).

## Conclusion

The process doc is the right operational companion to the design doc and is close to
ready for use. Fix the `RUST_SPLIT_INSTALL_DIR` / binary path mismatch before
automating the smoke tests in GitHub Actions; that is the only blocker for treating
the verification section as copy-paste ready.
