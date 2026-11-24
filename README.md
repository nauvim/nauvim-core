# nauvim-core

[![crates.io](https://img.shields.io/crates/v/nauvim-core.svg)](https://crates.io/crates/nauvim-core)
[![docs.rs](https://docs.rs/nauvim-core/badge.svg)](https://docs.rs/nauvim-core)

UI-independent Rust implementation of the client engine defined by
`nauvim-spec`. It targets Neovim v0.12.6 on Linux x86-64 and aarch64, and macOS
Apple Silicon.

The engine evaluates Lua configuration, starts or reconnects to owned persistent
Neovim sessions, delivers UI frames, and handles Nauvim plugin RPCs. It includes
multiplexed SSH connections, local forwarding, host-to-client file copies,
HTTP(S) file serving, GUI window requests, and clipboard exchange. Applications
provide their own renderer and GUI windows. The separate Lua plugin is optional
for ordinary editor operation.

This implementation targets [`nauvim-spec v1.1.0`](https://github.com/nauvim/nauvim-spec/tree/v1.1.0),
commit [`0e2456ae8e89370d170a99d4215ccc047fa02236`](https://github.com/nauvim/nauvim-spec/commit/0e2456ae8e89370d170a99d4215ccc047fa02236).

When the referenced `nauvim-spec` commit hash changes, `nauvim-core` increases its
minor or major version. When the hash stays the same, it increases its patch
version. Breaking changes or other release requirements may require exceptions
to these rules. In all cases, releases with the same minor version are guaranteed
to reference the same `nauvim-spec` commit hash.

Build with Cargo and a C compiler. Lua 5.4 is bundled with the crate.

The following console example prints errors and exits on the welcome or shutdown
event. It requires Neovim on `PATH` and a direct Tokio 1 dependency with `macros`
and `rt-multi-thread` enabled. See [`examples/events.rs`](examples/events.rs).
For a GUI driver that keeps destination selection available, use the
[frontend implementor guide](docs/implementors.md#runtime-and-event-loop-integration).

```rust
use nauvim_core::{Config, Engine, EngineEvent, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let (config, ()) = Config::load(None, "org.example.nauvim.events", |_opts| Ok(()))?;
    let mut engine = Engine::new(config)?;
    engine.start(100, 30).await?;

    loop {
        match engine.next_event().await {
            EngineEvent::Frame(_frame) => {
                // A GUI application presents the frame through its Backend.
            }
            EngineEvent::Error(error) => eprintln!("{error}"),
            EngineEvent::Welcome { error } => {
                if let Some(error) = error {
                    eprintln!("{error}");
                }
                break;
            }
            EngineEvent::Shutdown => break,
            _ => {}
        }
    }

    engine.shutdown().await
}
```

The console example ignores the selected UI options. A GUI frontend supplies its
documented implementation ID and parses the selected UI options in the callback;
see [UI-specific configuration](docs/implementors.md#ui-specific-configuration).

Use a Tokio runtime for engine operations. Call `shutdown()` when the last GUI
window closes. SIGTERM is handled while connecting or receiving events, and
produces `EngineEvent::Shutdown` after cleanup. Host Neovim processes and their
live RPC sockets survive detachment and shutdown.

All optional features are enabled by default. Use `default-features = false`
and select any combination of `ssh`, `plugin-local-forward`, `plugin-filesystem`,
`plugin-http`, and `plugin-clipboard`. Client registration and session switching
are always available.

Start with the [frontend implementor guide](docs/implementors.md) to build a
specification-compliant UI with any toolkit. It covers the event loop, rendering,
input, clipboard, plugin windows, recovery, and shutdown. See also the
[configuration reference](docs/usage.md),
[development and validation](docs/development.md), and the Rust API documentation.

Licensed under MIT OR Apache-2.0.
