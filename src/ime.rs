//! Neovim cursor information for frontend input method editor (IME) integration.
//!
//! The frontend owns native IME integration, including composition and
//! candidate placement. Send committed keyboard text with
//! [`Client::send_key()`](crate::Client::send_key) and [`KeyInput::Text`](crate::KeyInput::Text).

use crate::State;

use nvim_rpc_ui::{Cmdline, Cursor};

/// A Neovim text cursor that the frontend can locate in its rendered UI.
#[derive(Debug, Clone, Copy)]
pub enum Anchor<'a> {
    /// The cursor in a Neovim grid, in grid-local screen cells.
    Grid(Cursor),
    /// The cursor in the active external command line. Its `pos` is a UTF-8
    /// byte offset into `content`, before adding any displayed prefix.
    Cmdline(&'a Cmdline),
}

/// Returns the text cursor from a presented Neovim state snapshot.
///
/// The innermost external command line takes precedence over the grid cursor.
/// Returns `None` when the snapshot contains neither. The frontend decides
/// when to accept composition and maps the anchor through its own layout.
pub fn anchor(state: &State) -> Option<Anchor<'_>> {
    if let Some((_, cmdline)) = state.cmdlines.last_key_value() {
        return Some(Anchor::Cmdline(cmdline));
    }

    state.cursor.map(Anchor::Grid)
}
