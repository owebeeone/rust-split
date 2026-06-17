# rust-split release process plan

This plan describes how to cut releases for `rust-split` once `cargo-dist` is
configured.

For the design context, minimal config, and uv reference boundary, see
`dev-docs/RSplitRelease.md`.

## One-Time Setup

1. Ensure this is a git repository with at least one commit. `cargo-dist`
   needs git metadata for release planning and source archives.
2. Add `repository`, `[package.metadata.dist]`, and `[profile.dist]` to
   `Cargo.toml`.
3. Configure GitHub artifact attestations in `dist-workspace.toml`.
4. Run `dist init`.
5. Enable the two installers: shell and PowerShell.
6. Keep the initial target set small:
   - `aarch64-apple-darwin`
   - `x86_64-apple-darwin`
   - `aarch64-unknown-linux-gnu`
   - `x86_64-unknown-linux-gnu`
   - `x86_64-pc-windows-msvc`
7. Confirm `dist generate` ran. `dist init` normally does this automatically.
8. Commit the generated files:
   - `Cargo.toml`
   - `dist-workspace.toml`
   - `.github/workflows/release.yml`

Expected `Cargo.toml` additions:

```toml
repository = "https://github.com/owebeeone/rust-split"

[package.metadata.dist]
dist = true

[profile.dist]
inherits = "release"
lto = "thin"
```

Expected local setup commands:

```sh
cargo install cargo-dist --version 0.31.0
dist init --yes \
  --installer shell \
  --installer powershell \
  --target aarch64-apple-darwin \
  --target x86_64-apple-darwin \
  --target aarch64-unknown-linux-gnu \
  --target x86_64-unknown-linux-gnu \
  --target x86_64-pc-windows-msvc
dist plan
```

Use the same cargo-dist version locally as `cargo-dist-version` in
`dist-workspace.toml`.

Expected `dist-workspace.toml` signing settings:

```toml
[dist]
github-attestations = true
github-attestations-phase = "announce"
github-attestations-filters = [
    "*.json",
    "*.sh",
    "*.ps1",
    "*.zip",
    "*.tar.xz",
    "*.sha256",
    "sha256.sum",
]
```

If config changes later, regenerate and re-check:

```sh
dist generate
dist plan
```

Pull requests should run `dist plan` only by default. Tag pushes run the full
release build and upload.

## Release Checklist

1. Pick the version, for example `0.1.0`.
2. Update `version` in `Cargo.toml`.
3. Update GitHub release notes or `README.md` release notes. Add
   `CHANGELOG.md` later if release notes become substantial.
4. Run local verification:

```sh
cargo test
cargo clippy --all-targets --all-features -- -D warnings
dist plan
```

5. Commit the release prep.
6. Tag the release:

```sh
version="0.1.0"
git tag "v${version}"
git push origin "v${version}"
```

7. Let the generated GitHub Actions release workflow build and upload artifacts.
8. On the first release, confirm the generated installer uses the expected
   `RUST_SPLIT_` environment variable names before trusting the smoke tests:

```sh
version="0.1.0"
tmp="$(mktemp -d)"
curl --proto '=https' --tlsv1.2 -LsSf \
  "https://github.com/owebeeone/rust-split/releases/download/v${version}/rust-split-installer.sh" \
  -o "${tmp}/rust-split-installer.sh"
grep -q 'RUST_SPLIT_UNMANAGED_INSTALL' "${tmp}/rust-split-installer.sh"
grep -q 'RUST_SPLIT_NO_MODIFY_PATH' "${tmp}/rust-split-installer.sh"
grep -q 'RUST_SPLIT_DOWNLOAD_URL' "${tmp}/rust-split-installer.sh"
```

9. Verify at least one release asset has a GitHub artifact attestation:

```sh
version="0.1.0"
tmp="$(mktemp -d)"
gh release download "v${version}" \
  --repo owebeeone/rust-split \
  --pattern 'rust-split-x86_64-unknown-linux-gnu.tar.xz' \
  --dir "${tmp}"
gh attestation verify \
  "${tmp}/rust-split-x86_64-unknown-linux-gnu.tar.xz" \
  --repo owebeeone/rust-split
```

10. After the assets are published, run the installer smoke tests below. The first
   release has to complete CI before these tests can pass, because the scripts
   install from the public GitHub Release URLs.

## Expected Release Assets

The GitHub Release for the selected tag should contain:

- `rust-split-aarch64-apple-darwin.tar.xz`
- `rust-split-x86_64-apple-darwin.tar.xz`
- `rust-split-aarch64-unknown-linux-gnu.tar.xz`
- `rust-split-x86_64-unknown-linux-gnu.tar.xz`
- `rust-split-x86_64-pc-windows-msvc.zip`
- per-artifact `*.sha256` files
- `sha256.sum`
- GitHub artifact attestations
- `rust-split-installer.sh`
- `rust-split-installer.ps1`
- source archive

The exact archive extension can change if `unix-archive` is configured
differently. Keep the default `.tar.xz` unless there is a reason to change it;
the uv reference sample uses `.tar.gz` because uv overrides the default.

## Installer Verification

The release is not done until both install scripts are smoke-tested against the
published assets.

Do not run the checked-in `installer/install.sh` or `installer/install.ps1` files
for this verification. They are uv reference samples. The real installers are
the generated `rust-split-installer.sh` and `rust-split-installer.ps1` release
assets.

Set the version once before running the smoke test.

Unix shell:

```sh
export RUST_SPLIT_VERSION=0.1.0
```

PowerShell:

```powershell
$env:RUST_SPLIT_VERSION = "0.1.0"
```

The smoke test contract is:

1. install into a temporary directory
2. do not modify PATH
3. run the installed binary
4. verify the version
5. run one tiny functional command

The smoke tests use `RUST_SPLIT_UNMANAGED_INSTALL`, which installs the binary
directly into the requested directory and avoids PATH edits. The `RUST_SPLIT_*`
names follow cargo-dist's app-name convention; confirm them against the first
generated installer as described in the release checklist.

The version checks assume clap's default `--version` output:
`rust-split <version>`.

### Unix Installer

Run this on macOS and Linux after the release assets exist:

```sh
set -eu

: "${RUST_SPLIT_VERSION:?set RUST_SPLIT_VERSION, e.g. 0.1.0}"
version="${RUST_SPLIT_VERSION}"
base_url="https://github.com/owebeeone/rust-split/releases/download/v${version}"
tmp="$(mktemp -d)"

curl --proto '=https' --tlsv1.2 -LsSf \
  "${base_url}/rust-split-installer.sh" \
  -o "${tmp}/rust-split-installer.sh"

RUST_SPLIT_UNMANAGED_INSTALL="${tmp}/bin" \
RUST_SPLIT_NO_MODIFY_PATH=1 \
sh "${tmp}/rust-split-installer.sh"

"${tmp}/bin/rust-split" --version | grep -q "rust-split ${version}"
"${tmp}/bin/rust-split" --help | grep -q "explode"

sample="${tmp}/sample.rs"
out="${tmp}/chunks"
printf 'fn main() {}\n' > "${sample}"
"${tmp}/bin/rust-split" explode "${sample}" --out "${out}"
test -f "${out}/manifest.toml"
```

The installer chooses the platform archive and verifies checksums when the
generated release metadata provides them.

### Windows Installer

Run this on `windows-latest` after the release assets exist:

```powershell
$ErrorActionPreference = "Stop"

if (-not $env:RUST_SPLIT_VERSION) {
    throw "Set RUST_SPLIT_VERSION, e.g. 0.1.0"
}
$version = $env:RUST_SPLIT_VERSION
$baseUrl = "https://github.com/owebeeone/rust-split/releases/download/v$version"
$tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "rust-split-release-test")
$installer = Join-Path $tmp "rust-split-installer.ps1"

Invoke-WebRequest "$baseUrl/rust-split-installer.ps1" -OutFile $installer

$env:RUST_SPLIT_UNMANAGED_INSTALL = Join-Path $tmp "bin"
$env:RUST_SPLIT_NO_MODIFY_PATH = "1"
Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
& $installer

$exe = Join-Path $env:RUST_SPLIT_UNMANAGED_INSTALL "rust-split.exe"
$versionOutput = & $exe --version
if (-not ($versionOutput | Select-String -SimpleMatch "rust-split $version")) {
    throw "version mismatch: $versionOutput"
}

$helpOutput = & $exe --help
if (-not ($helpOutput | Select-String -SimpleMatch "explode")) {
    throw "help missing 'explode'"
}

$sample = Join-Path $tmp "sample.rs"
$out = Join-Path $tmp "chunks"
Set-Content -Path $sample -Value "fn main() {}"
& $exe explode $sample --out $out
if (-not (Test-Path (Join-Path $out "manifest.toml"))) {
    throw "manifest.toml not produced"
}
```

## OS-Native Code Signing

GitHub artifact attestations are the first signing layer for this project. They
are not the same as OS-native code signing.

Add OS-native signing only when we are ready to manage the required credentials.

For Windows Authenticode signing:

1. Buy or provision an SSL.com eSigner code-signing certificate.
2. Add these GitHub secrets:
   - `SSLDOTCOM_USERNAME`
   - `SSLDOTCOM_PASSWORD`
   - `SSLDOTCOM_TOTP_SECRET`
   - `SSLDOTCOM_CREDENTIAL_ID`
3. Add the signing setting to the dist config:

```toml
[dist]
ssldotcom-windows-sign = "prod"
```

Use `"test"` instead of `"prod"` with SSL.com's sandbox certificate. After the
release, verify the downloaded Windows binary:

```powershell
Get-AuthenticodeSignature .\rust-split.exe
```

For macOS signing, plan a separate pass. It requires Apple Developer ID
credentials and a codesign/notarization flow; do not block the first release on
that unless macOS Gatekeeper behavior becomes a user-facing problem.

## CI Verification Job

Add a post-release workflow once the first release is working. It should:

- run on `workflow_dispatch` with a `version` input, or after the release workflow
- run the Unix smoke test on `ubuntu-latest`
- run the Unix smoke test on `macos-latest`
- run the PowerShell smoke test on `windows-latest`

This deliberately tests the same public install path users will use. Local
`dist build` verification is useful, but it does not prove the published URLs and
release asset names are correct.

## Local Dry Run

Before tagging, check what `cargo-dist` intends to publish:

```sh
dist plan
dist build
dist build --artifacts=global
ls target/distrib
```

Local `dist build` requires git metadata, including at least one commit, because
cargo-dist generates source archive metadata from the repository.

Local installer testing can be done by serving `target/distrib` over HTTP and
setting `RUST_SPLIT_DOWNLOAD_URL` to that local base URL. Keep the official gate
on the published GitHub Release, because that is the user-facing install path.

For a staging dry run, reuse the same smoke tests with:

```sh
export RUST_SPLIT_DOWNLOAD_URL=http://127.0.0.1:8000
```

and serve `target/distrib` from that URL.

## Failure Handling

- If an installer 404s, check the tag name, release asset names, and `repository`
  URL in `Cargo.toml`.
- If the release build fails with a version mismatch, the tag and `Cargo.toml`
  version are out of sync. cargo-dist enforces that agreement before shipping.
- If PATH changes during verification, make sure `RUST_SPLIT_NO_MODIFY_PATH=1`
  is set.
- If a platform is missing, add the target to `dist-workspace.toml`, run
  `dist generate`, and re-run `dist plan`.
- If a broken release is published, delete the GitHub Release and tag, remove
  the bad assets, fix the repo, then create and push the corrected tag again.
  Installers are version-pinned, so stale assets at the same tag can keep
  serving the broken build.
