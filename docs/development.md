# Development and validation

This crate uses Rust 2024 and published `nvim-rpc-plugin` and `nvim-rpc-ui`
dependencies. It does not require a sibling checkout of either crate. Run checks
from the `nauvim-core` directory.

## Regular checks

```sh
cargo fmt --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all-features
cargo test --no-default-features
cargo doc --all-features --no-deps
```

Feature-specific builds can be checked independently:

```sh
cargo check --no-default-features --features ssh
cargo check --no-default-features --features plugin-local-forward
cargo check --no-default-features --features plugin-filesystem
cargo check --no-default-features --features plugin-http
cargo check --no-default-features --features plugin-clipboard
```

Regular tests cover configuration, RPC dispatch, resource cancellation, local
forwarding, local copy behavior, HTTP responses, and clipboard backend selection
without requiring a host Neovim or desktop clipboard. Networking tests need
permission to bind local loopback ports and Unix sockets.

## Live integration checks

Live tests are ignored by default. Set `NVIM_BIN` to an absolute executable path
for Neovim v0.12.6. Session tests start isolated host processes and sockets.
Their host-side setup is supplied by test fixtures; a separate `nauvim-plugin`
checkout is not needed:

```sh
NVIM_BIN=/absolute/path/to/nvim cargo test --all-features --test session -- --ignored
```

The SSH test provisions a temporary loopback `sshd`, host keys, an identity, an
authorized-key file, and a known-hosts file. It requires the OpenSSH server
(`openssh-server` on Debian and Ubuntu), `ssh-keygen`, `kill`, SFTP support, and an
account permitted to authenticate to the test daemon. The test searches `PATH`
for `sshd` and falls back to `/usr/sbin/sshd`. Set `SSHD_BIN` to use a specific
executable. The test does not use the user's SSH configuration or host-key trust
file.

```sh
NVIM_BIN=/absolute/path/to/nvim cargo test --all-features --test ssh -- --ignored
```

The isolated clipboard integration test uses temporary command fixtures and
Neovim to check notification validation and native GUI paste. It does not access
the desktop clipboard. Run it with `NVIM_BIN` set to exercise the complete flow.

```sh
NVIM_BIN=/absolute/path/to/nvim cargo test --all-features --test clipboard isolated_notifications -- --ignored
```

The real Linux clipboard test requires `xclip`, an accessible X11 `DISPLAY`, and
permission to use local sockets. It temporarily changes both the clipboard and
primary selection, then restores their original bytes.

```sh
NVIM_BIN=/absolute/path/to/nvim cargo test --all-features --test clipboard live_copy_notifications -- --ignored
```

Report live suites separately from regular tests. A successful ignored-test
compilation does not validate Neovim, SSH authentication, or a real clipboard.
