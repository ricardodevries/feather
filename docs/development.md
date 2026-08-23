# Development and releases

Rust 1.96 is pinned in [`rust-toolchain.toml`](../rust-toolchain.toml). The workspace builds on macOS for local development. Linux hardware drivers and systemd packaging are compiled and tested in Docker and CI.

## Workspace

```text
crates/feather-core/  Configuration, curves, IPC, and status types
crates/featherd/      Policy engine, hardware drivers, socket server, and systemd integration
crates/feather-cli/   Local client and offline configuration commands; binary name: feather
```

The crate boundary keeps HID, hwmon, NVML, serial, and systemd dependencies out of the client. `feather-core` contains the versioned newline-delimited JSON protocol shared by both executables.

## Local checks

```sh
cargo fmt --all -- --check
cargo test --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --all-features
cargo audit
```

Build and test the deployable Linux amd64 bundle on Apple Silicon:

```sh
./scripts/build-amd64.sh
```

## Continuous integration

CI runs formatting, tests, Clippy, documentation checks, `cargo audit`, a release build, and `systemd-analyze verify` on Ubuntu. Action versions are pinned to commit hashes. Dependabot checks Cargo and GitHub Actions dependencies each week.

## Tagged releases

The release workflow runs for tags matching `v*`. It requires the tag to equal the workspace package version, builds the tested amd64 bundle, creates a compressed archive and checksum, and publishes both files to the GitHub release.

Prepare a release by changing the workspace version and `Cargo.lock`, updating the documentation, and running the local and Linux checks. Commit those changes before creating the tag:

```sh
git tag v0.1.2
git push origin main v0.1.2
```
