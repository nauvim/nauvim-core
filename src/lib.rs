#![cfg_attr(docsrs, feature(doc_cfg))]

//! UI-independent Nauvim sessions, Lua configuration, and client resources.
//!
//! Load [`Config`], construct an [`Engine`], and consume [`EngineEvent`] values
//! on the application's event loop. Host Neovim processes survive detachment
//! and engine shutdown. The application owns its toolkit and rendering objects.
//!
//! These APIs serve any specification-compliant frontend, independently of its
//! toolkit or repository. The source distribution includes a comprehensive
//! frontend integration guide in `docs/implementors.md`.
//!
//! # Application event loop
//!
//! [`Engine::start()`] applies startup host selection. Keep receiving events to
//! advance plugin-requested session switches and display cleanup errors.
//!
//! ```no_run
//! use nauvim_core::{Config, Engine, EngineEvent};
//!
//! # use nauvim_core::Error;
//! # async fn example() -> Result<(), Error> {
//! let (config, ()) = Config::load(None, "org.example.nauvim.events", |_opts| Ok(()))?;
//! let mut engine = Engine::new(config)?;
//! engine.start(100, 30).await?;
//!
//! loop {
//!     match engine.next_event().await {
//!         EngineEvent::Frame(frame) => {
//!             // Present the stable frame with the application's Backend.
//!             let _state = frame.state;
//!         }
//!         EngineEvent::Shutdown => break,
//!         _ => {} // Handle welcome pages, errors and plugin windows.
//!     }
//! }
//!
//! engine.shutdown().await?;
//! # Ok(())
//! # }
//! ```

mod checkpoint;
mod config;
pub use config::{
    ClipboardConfig, Config, HostInfo, HostSelection, Policies, ProjectDirs, UiConfig, UiTable,
    UiTableId, UiValue,
};
mod connection;
mod engine;
pub use engine::{Client, Engine, EngineEvent, WebviewPosition};
mod error;
pub use error::Error;
pub mod resources;
pub use resources::ResourceHandle;
#[cfg(feature = "plugin-http")]
pub use resources::WebviewWindow;
mod transport;
pub use transport::Transport;
mod dispatch;
mod socket;
#[cfg(feature = "ssh")]
mod ssh;

pub use nvim_rpc_ui::{Backend, Frame, GridId, KeyInput, Modifiers, SpecialKey, State, UiOptions};

use std::fmt;

fn is_spec_string(s: &str) -> bool {
    !s.starts_with('\u{feff}') && !s.contains('\0')
}

/// Identifies one Nauvim process across attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u32);

impl ClientId {
    /// Returns the unsigned integer used by the plugin RPC protocol.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl From<u32> for ClientId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<ClientId> for u32 {
    fn from(value: ClientId) -> Self {
        value.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
