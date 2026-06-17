# rust-split release design

This is the minimal release design for `rust-split`: ship a standalone binary
with generated Unix and Windows installers.

`../third-party/uv` is only a reference project. It shows the artifact shape we
want, but its release system is much larger than this project needs.

## Goal

Each release should publish:

- `rust-split` archives for the supported platforms
- SHA-256 checksums
- GitHub artifact attestations for the release assets
- `rust-split-installer.sh`
- `rust-split-installer.ps1`
- source archive generated from the git tag

End users should be able to install without a Rust toolchain:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/owebeeone/rust-split/releases/download/v0.1.0/rust-split-installer.sh | sh
```

```powershell
irm https://github.com/owebeeone/rust-split/releases/download/v0.1.0/rust-split-installer.ps1 | iex
```

Users who already have Rust can still use `cargo install rust-split`.

## Tool

Use `cargo-dist` for the GitHub release artifacts and generated installers.

The checked-in `installer/install.sh` and `installer/install.ps1` files are uv
installer samples. They demonstrate the generated script style, but they are not
release source files for this repo and should not be hand-edited for releases.

## Minimal Configuration

`Cargo.toml` needs repository metadata so generated installers know where release
assets live:

```toml
repository = "https://github.com/owebeeone/rust-split"

[package.metadata.dist]
dist = true

[profile.dist]
inherits = "release"
lto = "thin"
```

`dist-workspace.toml` should stay small:

```toml
[workspace]
members = ["cargo:."]

[dist]
cargo-dist-version = "0.31.0"
ci = "github"
installers = ["shell", "powershell"]
install-updater = false
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
targets = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
]
```

That is enough for a first release. Add more targets only when there is a real
user need.

## Signing Model

There are three different integrity layers:

1. **Checksums** prove the downloaded bytes match the release metadata.
2. **GitHub artifact attestations** prove release artifacts came from the GitHub
   Actions build for this repository.
3. **OS-native code signing** proves an executable was signed by an Apple or
   Windows code-signing identity.

For `rust-split`, enable checksums and GitHub artifact attestations from the
first release. Treat OS-native code signing as a later hardening step because it
requires paid or vendor-managed credentials:

- Windows Authenticode signing requires a code-signing certificate or cloud
  signing service. cargo-dist supports SSL.com eSigner via
  `ssldotcom-windows-sign`.
- macOS code signing requires an Apple Developer ID certificate, and practical
  distribution usually also involves notarization.
- Linux does not have one universal OS-native binary-signing gate. Use checksums
  and attestations for the GitHub release assets.

## uv Reference Boundary

Keep from uv:

- generated shell and PowerShell installers
- platform archive names
- checksum-backed downloads
- GitHub artifact attestations
- environment-variable install overrides

Do not copy uv's release system:

- CDN mirror hosting
- Docker/PyPI/docs publishing
- custom artifact builders
- release dispatch approval flow
- broad target matrix

Those are useful examples later, not requirements for `rust-split`.

## Installer Contract

Generated installers should support:

- choosing the matching archive for the host platform
- checksum verification when checksums are available
- installing into an override directory
- skipping PATH mutation for CI verification
- printing a working `rust-split --version` after install

For the concrete release checklist and installer smoke tests, see
`dev-docs/RSplitReleaseProcess.md`.
