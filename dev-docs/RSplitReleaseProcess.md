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
7. Run `dist generate --mode ci`.
8. Edit `.github/workflows/release.yml` so release builds trigger on
   `release.published`. Keep `workflow_dispatch` only as a repair/retry path for
   an already-published release. The workflow must not have `push` or
   `pull_request` triggers.
9. Add `allow-dirty = ["ci"]` to `dist-workspace.toml` after the workflow edit.
   This tells cargo-dist that the generated CI file is intentionally patched.
10. Commit the generated files:
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

Expected `dist-workspace.toml` release settings:

```toml
[dist]
allow-dirty = ["ci"]
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

If config changes later, regenerate and re-check. Because the workflow is
intentionally patched, temporarily remove `allow-dirty = ["ci"]`, regenerate,
apply the release-event trigger patch again, then restore `allow-dirty = ["ci"]`:

```sh
dist generate --mode ci
dist plan
```

After regenerating, reapply the trigger check to `.github/workflows/release.yml`.
Normal commits, pull requests, and tag pushes must not start release builds.
Release builds start when a GitHub Release is published. `workflow_dispatch`
exists only to retry or repair an already-published release.

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
6. Create and push the release tag:

```sh
version="0.1.0"
git tag "v${version}"
git push origin "v${version}"
```

7. Create a GitHub Release for the pushed tag and publish it.
8. Let the GitHub Actions release workflow build and upload artifacts to that
   existing release.
9. On the first release, confirm the generated installer uses the expected
   `RUST_SPLIT_` environment variable names before trusting the smoke tests.
   Also confirm the shell installer contains embedded checksum verification:

```sh
version="0.1.0"
tmp="$(mktemp -d)"
curl --proto '=https' --tlsv1.2 -LsSf \
  "https://github.com/owebeeone/rust-split/releases/download/v${version}/rust-split-installer.sh" \
  -o "${tmp}/rust-split-installer.sh"
grep -q 'RUST_SPLIT_UNMANAGED_INSTALL' "${tmp}/rust-split-installer.sh"
grep -q 'RUST_SPLIT_NO_MODIFY_PATH' "${tmp}/rust-split-installer.sh"
grep -q 'RUST_SPLIT_DOWNLOAD_URL' "${tmp}/rust-split-installer.sh"
grep -q 'verify_checksum' "${tmp}/rust-split-installer.sh"
grep -q '_checksum_value=' "${tmp}/rust-split-installer.sh"
```

10. On the first release, inspect the generated PowerShell installer before
   claiming that it verifies archive checksums:

```powershell
$version = "0.1.0"
$tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "rust-split-installer-check")
$installer = Join-Path $tmp "rust-split-installer.ps1"
Invoke-WebRequest `
    "https://github.com/owebeeone/rust-split/releases/download/v$version/rust-split-installer.ps1" `
    -OutFile $installer

$script = Get-Content $installer -Raw
if (-not ($script.Contains("RUST_SPLIT_UNMANAGED_INSTALL"))) {
    throw "PowerShell installer does not use expected RUST_SPLIT_UNMANAGED_INSTALL variable"
}
if (-not ($script.Contains("Get-FileHash") -or $script.Contains("checksum"))) {
    Write-Warning "PowerShell installer does not appear to verify archive checksums; document it as a convenience installer only."
}
```

11. Verify at least one release asset has a GitHub artifact attestation:

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

12. After the assets are published, run the installer smoke tests below. The first
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

The installer commands are convenience paths. `curl | sh` and `irm | iex` execute
the downloaded installer script before the user can verify that script. The
installer script can still verify the archive it downloads, but the script itself
is trusted through HTTPS and the GitHub Release URL unless the user downloads and
verifies it first.

The verified path is:

1. download the installer or archive
2. verify the downloaded file with `gh attestation verify`
3. compare the SHA-256 checksum for archives
4. run the installer or unpack the archive

The release notes should describe this as "checksummed and attested release
assets," not "signed binaries."

Do not run any uv reference installer samples for this verification. The real
installers are the generated `rust-split-installer.sh` and
`rust-split-installer.ps1` release assets.

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

### Verified Unix Install

For users who do not want to pipe directly into `sh`, download and verify the
installer before running it:

```sh
set -eu

version="0.1.0"
tmp="$(mktemp -d)"
installer="${tmp}/rust-split-installer.sh"

gh release download "v${version}" \
  --repo owebeeone/rust-split \
  --pattern 'rust-split-installer.sh' \
  --dir "${tmp}"
gh attestation verify "${installer}" --repo owebeeone/rust-split
sh "${installer}"
```

The generated cargo-dist shell installer embeds SHA-256 values for platform
archives and verifies the selected archive before unpacking when the required
checksum command is available.

### Verified Windows Archive Install

Until the generated PowerShell installer has an explicit checksum check, use the
archive as the verified Windows install path:

```powershell
$version = "0.1.0"
$repo = "owebeeone/rust-split"
$tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "rust-split-verified")
$zip = Join-Path $tmp "rust-split-x86_64-pc-windows-msvc.zip"
$checksum = Join-Path $tmp "rust-split-x86_64-pc-windows-msvc.zip.sha256"
$installDir = Join-Path $env:USERPROFILE ".local\bin"

gh release download "v$version" `
    --repo $repo `
    --pattern "rust-split-x86_64-pc-windows-msvc.zip" `
    --pattern "rust-split-x86_64-pc-windows-msvc.zip.sha256" `
    --dir $tmp
gh attestation verify $zip --repo $repo

$expected = (Get-Content $checksum).Split()[0].ToLowerInvariant()
$actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) {
    throw "checksum mismatch: expected $expected got $actual"
}

New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Expand-Archive -Force -Path $zip -DestinationPath $tmp
Copy-Item -Force (Join-Path $tmp "rust-split.exe") $installDir
```

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

The installer chooses the platform archive. The cargo-dist shell installer embeds
archive checksums and verifies them before unpacking when the required checksum
command is available.

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
