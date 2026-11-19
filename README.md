# nauvim-core

[![crates.io](https://img.shields.io/crates/v/nauvim-core.svg)](https://crates.io/crates/nauvim-core)
[![docs.rs](https://docs.rs/nauvim-core/badge.svg)](https://docs.rs/nauvim-core)

UI-independent Rust engine for Nauvim, targeting Neovim v0.12.6 on Linux
x86-64 and aarch64, and macOS Apple Silicon.

The engine evaluates Lua configuration, starts or reconnects to owned persistent
Neovim sessions, delivers UI frames, and handles Nauvim plugin RPCs. It includes
multiplexed SSH connections, local forwarding, host-to-client file copies,
HTTP(S) file serving, GUI window requests, and clipboard exchange. The application
provides its renderer and GUI windows; the separate Lua plugin is optional for
ordinary editor operation.

This implementation targets `nauvim-spec` commit
[`e65f68cba1fb5c42996ba0370f5a1b86537b6142`](https://github.com/nauvim/nauvim-spec/tree/e65f68cba1fb5c42996ba0370f5a1b86537b6142).

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
    let mut engine = Engine::new(Config::load(None)?)?;
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
