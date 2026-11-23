# Frontend implementor guide

This guide is for frontend developers using `nauvim-core`. You choose the
toolkit and renderer.

The [Nauvim specification][spec] defines the required behaviour. This guide
explains how to provide that behaviour through this crate's public API.
The supported host Neovim version is v0.12.6. The specified client and host
targets are Linux x86-64 and aarch64 and macOS Apple Silicon. The specification
covers one application process per client environment.

Use the [configuration reference](usage.md) for Lua settings and the crate's
rustdoc for individual methods. The [nvim-rpc-ui reference][ui-crate] describes
the rendering model and input types re-exported by `nauvim-core`.

- [Responsibilities and lifetimes](#responsibilities-and-lifetimes)
- [Runtime and event-loop integration](#runtime-and-event-loop-integration)
- [Startup and host selection](#startup-and-host-selection)
- [UI-specific configuration](#ui-specific-configuration)
- [Engine events and session transitions](#engine-events-and-session-transitions)
- [Rendering frames](#rendering-frames)
- [Keyboard, mouse, and focus](#keyboard-mouse-and-focus)
- [Clipboard and GUI paste](#clipboard-and-gui-paste)
- [Plugin windows and client resources](#plugin-windows-and-client-resources)
- [Errors, detachment, and shutdown](#errors-detachment-and-shutdown)
- [Frontend validation](#frontend-validation)

## Responsibilities and lifetimes

The host environment runs Neovim. The client environment runs your application,
its desktop clipboard, and its GUI components. A local host can be in the same
environment as the client; an SSH host is remote.

| Component | Responsibilities |
| --- | --- |
| `Engine` | Configuration, owned-session discovery and startup, UI attachment, RPC transport, SSH, plugin argument validation, and client resource cleanup. |
| Your `Backend` implementation | Presentation of Neovim frame state and ordered frame events through your renderer. |
| Your application | UI implementation ID and option schema, toolkit event loop, welcome page, host-selection controls, keyboard and GUI actions, fonts, GUI windows and tabs, error display, and process exit. |
| Host `nauvim-plugin` | Host-side Lua entry points and clipboard-provider callbacks. Core registers and selects its RPC destination during attachment. |

`Engine::new()` does not take a backend. On `EngineEvent::Frame`, your
application passes the owned `Frame` to `Backend::present()` on the thread that
owns the toolkit objects. A backend may retain the frame and schedule drawing
for a later toolkit paint callback.

Use one engine for the application's current attachment. It manages at most one
active host UI at a time, while cleanup from an earlier attachment can continue.
A Neovim tabpage or window is not a GUI tab or window. A frontend chooses how
those editor objects and UI grids appear in its own windows.

| Lifetime | Objects and behaviour |
| --- | --- |
| Host session | The owned Neovim process and its live RPC socket survive client detachment and application shutdown. |
| Application process | The engine's `ClientId`, configuration, and unique process temporary directory persist through host switches and recovery. |
| Attachment | The input `Client`, RPC and SSH connections, plugin resources, and plugin-created windows belong to that attachment. |
| Received frame | Its owned state snapshot remains usable after subsequent frames and after disconnection. Your application controls how long it retains snapshots. |

Keep `ClientId`, `ResourceHandle`, `GridId`, RPC channel IDs, and Neovim object
handles distinct in your adapter. For example, index plugin windows by
`ResourceHandle`, not by their target `GridId`.

## Runtime and event-loop integration

Run engine operations on a Tokio runtime with I/O and time enabled. The engine
also uses Tokio's process and Unix signal support. Your application needs a
direct Tokio dependency for the runtime and any Tokio APIs it calls. For
example, with the crate's default features:

```toml
[dependencies]
nauvim-core = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "time"] }
```

Building the crate requires a C compiler for its bundled Lua 5.4. No client-side
Neovim process is needed to evaluate configuration. See
[optional features](usage.md#optional-features) for selecting engine facilities.
Client registration and plugin-requested host switching are always available.

A common arrangement gives one asynchronous driver ownership of `Engine`.
The toolkit sends commands to that driver, and the driver sends engine events
back to the toolkit. Keep toolkit objects on their required thread. `Backend`
does not require `Send` or `Sync`; a renderer need not move to the engine's
runtime thread. A single-threaded arrangement is also possible when the toolkit
and Tokio integration allow both loops to keep making progress.

The driver needs to:

1. Apply startup host selection with `Engine::start()`.
2. Keep calling `Engine::next_event()`, including while displaying the welcome
   page or waiting for a plugin-requested connection.
3. Accept application commands for host selection, detachment, resizing, and
   shutdown.
4. Run input and paste futures without blocking toolkit painting.
5. Keep the runtime alive until explicit engine shutdown has finished.

Do not hold a synchronous toolkit callback open while waiting for SSH,
attachment, or clipboard operations. Serialize host selection, resizing,
detachment, and shutdown in the engine driver. Cloned input clients and owned
paste futures let input proceed alongside event reception.

Cancelling an ordinary pending `next_event()` wait preserves queued events and
an already-started connection attempt. Cancelling the wait for `start()` or
`connect()` also leaves an already-started attempt owned by the engine. Resume
event reception to publish its result. Use `detach()` to cancel a pending
attempt, or select another host to supersede it.

Give shutdown a separate finalization path: call and await `shutdown()` without
cancelling that future. In particular, event reception can itself be performing
SIGTERM cleanup when a command branch wins a `select!`. Always complete explicit
shutdown before treating the engine as finalized.

## Startup and host selection

Load configuration with `Config::load()`, supplying an optional path, your UI
implementation ID, and a parser for the selected UI-option dictionary. It
returns the core configuration and the parser's result separately. See
[UI-specific configuration](#ui-specific-configuration) for this interface.

Loading or validation failure selects the complete default configuration. The
engine emits retained diagnostics as client error events. An invalid clipboard
timeout is a narrower fallback: it retains the rest of the configuration and
uses the default timeout. See [loading configuration](usage.md#loading-configuration)
and [clipboard settings](usage.md#clipboard).

Create the engine, measure the editor viewport in cells, and call
`Engine::start(width, height)`. Both dimensions must be positive.

| Configuration | Action taken by `start()` |
| --- | --- |
| No host entries and no `default_host` | Connect to the independent default local host. |
| Host entries and no `default_host` | Emit `Welcome` and wait for user selection. |
| A non-empty `default_host` alias | Connect to that alias. |

Build the welcome page from `Config::hosts()` plus a separate default-local-host
choice. `Engine::connect_default()` selects that independent choice.
`Engine::connect(alias, width, height)` selects a named host. Aliases named
`local` and `default` are ordinary aliases, not reserved choices. An alias
absent from the configuration uses SSH resolution when SSH is enabled.

The same connection operation discovers the owned session, reuses it if it is
eligible, or starts a host Neovim if needed. The current public API does not
enumerate arbitrary running Neovim processes or offer a force-new-session flag.
Present the operation as selecting a destination, rather than promising a list
of every running editor or a choice to replace a live owned session.

Startup selection runs once. After detachment, host exit, or recovery, keep the
welcome page usable and use `connect()` or `connect_default()` for another
selection or retry. Calling `start()` again does not repeat automatic startup.

A failed connection is recoverable. Do not propagate its error out of the
application's main loop and exit. Connection-attempt failures normally both
return an error and queue `Welcome { error: Some(...) }`. Choose one display path
for that diagnostic. Also handle the returned result: errors such as failing
to install the startup signal handler can occur before a welcome event is
queued.

## UI-specific configuration

Choose and document a stable UI implementation ID for your application, together
with its option schema: field names, types, meanings, and defaults. The ID
identifies the implementation across launches. Reverse domain name notation,
such as `org.example.nauvim.gui`, is recommended. It must be non-empty valid
UTF-8, must not begin with U+FEFF, and must not contain NUL. Core rejects an
invalid supplied ID before reading or executing configuration.

`Config::load()` passes an owned `UiConfig` containing the selected dictionary to
your parser after common configuration validation. It contains pure Rust data
and can outlive the core configuration and engine. Parse it into your
application's settings and return schema failures as `nauvim_core::Error`. Core
returns `(Config, T)`, where `T` is your parser's result type. Use `Ok` as the
parser to receive `UiConfig` directly. See
[UI-specific settings](usage.md#ui-specific-configuration) for mapping validation
and exact selection rules.

Read root fields with `UiConfig::get()`. `UiValue::String` holds validated Rust
text. Resolve a nested table reference with `UiConfig::table()`. Use
`UiTable::is_list()` to identify non-empty lists. Their `entries()` yield
one-based integer keys in order. Dictionary entries have string keys and
iterate in unspecified order. An empty Lua table becomes an empty dictionary.

The following partial example defines an illustrative schema with one optional
positive integer field. It runs before engine construction:

```rust
use nauvim_core::{Config, Engine, Error, UiConfig, UiValue};

struct GuiOptions {
    font_size: i64,
}

fn parse_ui_opts(opts: UiConfig) -> Result<GuiOptions, Error> {
    let font_size = match opts.get("font_size") {
        None => 12,
        Some(UiValue::Integer(n)) if *n > 0 => *n,
        _ => return Err(Error::Message("font_size must be a positive integer".into())),
    };

    Ok(GuiOptions { font_size })
}

fn create_engine() -> Result<(Engine, GuiOptions), Error> {
    let (config, opts) = Config::load(None, "org.example.nauvim.gui", parse_ui_opts)?;
    Ok((Engine::new(config)?, opts))
}
```

The parser runs only for the selected dictionary. Common validation also checks
unselected dictionaries, but their fields are not interpreted against your
schema. UI options remain distinct from Neovim UI attachment options.

If reading, evaluation, common validation, or your parser fails, `Config::load()`
discards the configuration and invokes your parser with an empty dictionary.
It returns complete core defaults, your parsed UI defaults, and the original
error in `Config::diagnostics()`. Your parser must accept an empty dictionary to
construct those defaults. If it rejects that dictionary, loading returns an
error. `Config::evaluate()` accepts the same ID and parser interface but returns
evaluation or schema errors directly, without the file-loading fallback.

## Engine events and session transitions

Handle every `EngineEvent` variant. There is no separate connection-success
event. A completed explicit connection makes `Engine::client()` available;
frames supply the editor's visual state. With plugin-requested connections,
continue event reception and obtain the current client when handling editor
actions.

| Event | Frontend action |
| --- | --- |
| `Frame(frame)` | Pass the frame to your rendering adapter. Preserve its ordered application events. |
| `Welcome { error }` | Show destination selection, reset attachment-specific rendering and input state, and display the optional error. Keep receiving events. |
| `Connect { host }` | Show the destination or connection progress. Continue receiving events to start the connection. |
| `Error(message)` | Display a client error. This event alone does not end an attachment. |
| `OpenWebview` | Begin creating the requested GUI window and retain its `WebviewWindow` by resource handle. |
| `FocusWebview { handle }` | Focus the window if it is still open and focusable. |
| `NavigateWebview` | Navigate the selected open window using the requested history and focus options. |
| `CloseWebview { handle }` | Close that presentation if it exists; tolerate a handle with no local window. |
| `Shutdown` | Finish application finalization, including an awaited `shutdown()`, then exit. |

The following is a **partial dispatcher** inside an asynchronous application
driver built with `plugin-http`. `ui` is your adapter. Its `present()` delegates to its `Backend`
and displays presentation failures; the other methods update toolkit state or
start window work. They do not await navigation or perform long-running work.
The surrounding driver also handles application commands and final shutdown.

```rust
use nauvim_core::EngineEvent;

loop {
    match engine.next_event().await {
        EngineEvent::Frame(frame) => ui.present(frame),
        EngineEvent::Welcome { error } => ui.show_welcome(error),
        EngineEvent::Connect { host } => ui.show_connecting(host),
        EngineEvent::Error(message) => ui.show_error(message),
        EngineEvent::OpenWebview {
            handle, url, position, focus, focusable, close_on_blur, window,
        } => {
            ui.open_webview(handle, url, position, focus, focusable, close_on_blur, window);
        }
        EngineEvent::FocusWebview { handle } => ui.focus_webview(handle),
        EngineEvent::NavigateWebview { handle, url, replace_history, focus, window } => {
            ui.navigate_webview(handle, url, replace_history, focus, window);
        }
        EngineEvent::CloseWebview { handle } => ui.close_webview(handle),
        EngineEvent::Shutdown => break,
    }
}
```

A plugin host switch produces `Welcome`, then `Connect { host }`. The next call
to `Engine::next_event()` starts the destination connection. To supply a
checkpoint, explicitly connect to the host alias with a
[checkpoint method](#restoring-ui-checkpoints) before receiving another event.
The explicit connection supersedes the pending automatic connection. Cleanup events for the previous attachment can arrive while the
destination is connecting or already active. Continue applying those events
to the windows identified by their handles.

This engine event differs from `nvim_rpc_ui::Event::Connect` and `Restart` in
`Frame::events`. Those carry Neovim server addresses; core surfaces them without
automatically switching sessions. `Engine::connect()` takes a host alias,
so keep the address-carrying frame events distinct from alias selection.

For a host selection initiated by your own UI, reset the renderer's old-session
state and cached input client when you submit the command. A successful direct
`connect()` does not need to emit a welcome event between attachments. Client
clones remain addressed to the attachment from which they were obtained; they
do not retarget themselves to a new host.

## Rendering frames

Implement the re-exported `Backend` trait:

```text
type Error;
fn present(&mut self, frame: Frame) -> Result<(), Self::Error>;
```

The frontend owns presentation failures and displays them in the client UI.
The engine does not call the backend or automatically receive its error value.

### Restoring UI checkpoints

Call `Engine::checkpoint()` while the attachment is active to capture its
latest committed UI state, including hidden grids. The checkpoint can include
redraws whose frames the frontend has yet to receive. The first redraw flush
makes a checkpoint available for fresh attachments and those using a checkpoint
as a reference. Capture a checkpoint before ending the attachment.

The caller owns checkpoint storage and must match a checkpoint to the same
running Neovim process that produced it. A host alias identifies a connection
destination and can point to a replacement process. Use `Checkpoint::encode()`
and `Checkpoint::decode()` to convert between checkpoints and bytes. See the
[nvim-rpc-ui reference][ui-crate].

Choose how to apply the checkpoint.

| Connection selection | Restore the saved snapshot | Use the checkpoint as a reference |
| --- | --- | --- |
| Host alias | `Engine::connect_with_checkpoint()` | `Engine::connect_with_checkpoint_reference()` |
| Default local host | `Engine::connect_default_with_checkpoint()` | `Engine::connect_default_with_checkpoint_reference()` |
| Startup selection | `Engine::start_with_checkpoint()` | `Engine::start_with_checkpoint_reference()` |

Snapshot restoration seeds the UI model with saved state. Reference mode starts
with an empty model and uses the checkpoint to supply missing data for current
redraw events. The [UI model documentation][ui-crate] defines which data can be
recovered.

Both modes reattach to a running Neovim. A missing session or an invalid
checkpoint follows the regular connection failure path. Use the ordinary
connection methods to start a fresh session. The engine passes the checkpoint
to the receiving UI model before attaching the UI.

The first redraw frame requests a full repaint. Present it through the ordinary
frame path. Its events come from the new attachment.
Restoration uses new connections and attachment resources. The outgoing
attachment follows the [ordinary cleanup procedure](#errors-detachment-and-shutdown).

### Enabled UI extensions

Core attaches with all nonlegacy UI capabilities enabled. In this version these
include `ext_linegrid`, `ext_multigrid`, `ext_hlstate`, `ext_termcolors`,
`ext_cmdline`, `ext_popupmenu`, `ext_tabline`, and `ext_messages`, with RGB colours.
The deprecated `ext_wildmenu` capability is not enabled.

Your renderer therefore needs the external widgets as well as the cell grids:

| State | Presentation work |
| --- | --- |
| `grids`, `placements`, `viewports` | Cell content, normal and floating grids, hidden grids, external windows, message-grid placement, stacking, and viewport information. |
| `highlights`, `highlight_groups`, `default_colors` | RGB styles, default-colour fallback, and semantic highlight information. |
| `cursor`, `mode`, `modes`, `busy` | Cursor position and appearance, mode changes, and busy state. |
| `cmdlines`, `cmdline_block` | Nested command lines, prompts, cursor positions, special characters, and command-line blocks. |
| `popupmenu` | Completion items, selection, and either a grid or command-line anchor. |
| `tabline` | Neovim tabpage and buffer entries and the current selections. |
| `messages` | Visible messages, showmode, showcmd, ruler, and message history. |

The generic `nvim-rpc-ui` documentation also serves applications that negotiate
fewer capabilities. `Engine` exposes no UI-option, connection-limit, or internal
frame-coalescing setter. Its re-export of `UiOptions` does not provide such a
setter. Plan a complete renderer for these enabled capabilities.

### Coordinates, text, and fonts

Read complete current state from `Frame::state`. A frame is a stable owned
snapshot with shared grid and row storage; the frontend does not need to replay
raw redraw notifications to reconstruct it.

Grid dimensions, cursor positions, and grid placements use screen cells.
Floating placements can contain fractional offsets. Grid content and placement
are separate: a hidden or closed window can leave its grid content retained in
the model. Use placement visibility when deciding what to display, and remove
rendering objects when grids disappear from state.

Cell text is UTF-8. An empty cell string marks the continuation of a double-width
character; it is not an independent printable blank. Honour the cell layout
when shaping and clipping text. Command-line cursor positions and command-line
popup anchors use UTF-8 byte offsets, not cell or Unicode-character indices.
Convert them through the displayed text's layout before positioning toolkit
objects. The [UI protocol][ui-protocol] defines the underlying event semantics.

Apply highlight attributes and resolve absent colours through
`State::default_colors`. When the model leaves a default colour unset, choose
the frontend's default. Font configuration is part of your
[UI-specific schema](#ui-specific-configuration). Font discovery, fallback,
shaping, metrics, and rendering belong to the application; core supplies no
font-search or font-diagnostic service.
Font or display-scale changes require updating cell metrics, layout, and the
requested UI dimensions.

Use `Engine::resize(width, height)` for a measured viewport change. It retains
the size for later plugin switches as well as notifying the active host.
Render the dimensions in subsequent frame grids: completion of a resize request
does not mean Neovim has already applied it. If the viewport changes during an
in-flight connection, apply the latest size after attachment; an attempt already
started with its original dimensions.

### Damage, deferred painting, and ordered events

`Frame::damage` describes changes from the preceding published frame, or the
combined changes when delivery coalesces frames. Grid damage rectangles use
half-open `top..bottom` and `left..right` ranges. The `layout`, `cursor`, and
`widgets` flags identify presentation updates separate from cell damage.
A full repaint also requires discarding grids and placements absent from the
latest state. Core's internal delivery retains one pending frame and combines
updates when the frontend falls behind. Its default coalescing requests a full
repaint for combined frames.

A simple backend can paint each received snapshot in full. To defer or combine
painting in your own adapter:

1. Process every frame's `events` in receive order, exactly once. Bells, title
   changes, and asynchronous RPC errors are examples of events separate from
   cell painting. Assign event handling to one layer of your adapter.
2. Retain the latest snapshot and the snapshot last successfully painted.
3. When painting, use `State::diff_since()` against the last painted snapshot,
   or request a full repaint. Do not use only the last received frame's damage
   after skipping intermediate paints.
4. Advance the painted baseline only after successful drawing. Repaint from the
   retained snapshot after toolkit exposure, font changes, or graphics-resource
   loss, even if no new engine frame arrives.

`Frame::coalesce()` and `Frame::coalesce_incremental()` are alternatives when
retaining pending frames: they combine ordered events as well as state. Use
receive order within one attachment and handle their event-limit errors.
Reset attachment-specific renderer caches and repaint on a host transition.
Bound snapshots and pending events retained by your application; dependency
delivery limits do not bound memory held by your own queues.

## Keyboard, mouse, and focus

Obtain an input client with `Engine::client()`. It is `None` without an active
attachment. Use the current client for each editor action, or replace cached
clones on an attachment transition.

Translate toolkit keyboard events into `KeyInput`. Use `Character` or the
`from_char()` helper for a layout-translated character, `SpecialKey` for named
keys, and `Modifiers` for logical modifiers. Do not apply physical Shift again
to a character already translated by the keyboard layout.

Use `KeyInput::Text` for committed literal text and `KeyInput::Sequence` only
when the source intentionally contains Neovim key notation. For example, this
helper sends the characters `<Esc>` rather than an Escape key:

```rust
use nauvim_core::{Client, Error, KeyInput};

async fn send_literal_text(client: &Client) -> Result<(), Error> {
    client.send_key(KeyInput::Text { text: "<Esc>" }).await?;

    Ok(())
}
```

`Client::input()` interprets Neovim key notation. Calls through a client and its
clones serialize complete keyboard sequences and retry partial input acceptance.
Completion means Neovim accepted the input, not that it executed every key.
Cancelling a call does not retract accepted input. Keep the application's input
command order when scheduling these futures.

Accept mouse input for GUI actions. Do not send Neovim mouse events
through `Client::mouse()`, encoded input sequences, or other Neovim mouse-input
facilities. This restriction applies regardless of `State::mouse_enabled` or the
host's `mouse` option. Clicking GUI controls can instead invoke actions such as
`Client::select_popupmenu_item()` or `Client::set_current_tabpage()`. If you
provide mouse text selection in editor grids, keep selections independent per
grid and include only that grid's text. These interaction rules also apply to
plugin-created GUI windows.

Report application focus changes through `Client::focus()`. It returns after
writing the notification, without waiting for editor execution. Native IME
integration is outside the specification's current scope; host plugins can
provide input methods.

## Clipboard and GUI paste

With `plugin-clipboard` enabled, core selects a client-side clipboard backend
and validates host copy notifications. It passes their text and requested
register (`+` or `*`) to that backend. Register-to-selection mapping depends on
the backend; `pbcopy` uses the same macOS clipboard for both registers.
The host does not need clipboard utilities. The frontend needs to display
backend-detection and operation errors, including errors when no host is
attached. Backend selection and timeout settings are described in the
[configuration reference](usage.md#clipboard).

Copy notifications carry the register and one UTF-8 text string. Core checks
the client ID and arguments before accessing the backend, including for RPCs
constructed without the Lua plugin. Valid copies preserve the supplied text,
including a trailing LF. Invalid notifications perform no backend operation
and produce no RPC response.

Bind the application's GUI paste action to `Engine::paste()`. Choose bindings
natural for the client platform; the specification does not prescribe a
particular shortcut. Consume the GUI action before it becomes an ordinary key
sent to Neovim.

`Engine::paste()` reads the client backend's `+` selection directly and sends
literal text through Neovim's native paste interface. It does not read text
through the host clipboard provider. That provider's paste callbacks supply no
text; reading a host clipboard register is not the GUI paste path. Do not turn
clipboard payloads into `Client::input()` key sequences.

The paste future owns its handles and captures the attachment when
`Engine::paste()` is called. It can run alongside event reception without
borrowing the engine. For example, inside an asynchronous driver with
`plugin-clipboard` enabled:

```rust
let paste = engine.paste();
let pending_paste = tokio::spawn(paste);
```

Retain and observe the task result, and stop or finish application-owned tasks
during shutdown. This example only launches the task; event reception and task
completion belong to the surrounding driver. A host switch does not redirect
that pending paste to the new attachment.

`Ok(true)` means Neovim accepted the paste; `Ok(false)` means its paste handler
cancelled it. CR, LF, and CRLF become line breaks. Failures return an error
and emit `EngineEvent::Error`. Select one error-display path to avoid duplicate
messages.
`Client::paste(text)` is the lower-level operation for text the caller already
has.

## Plugin windows and client resources

Plugin support is optional for ordinary editor operation. Core performs client
registration and asynchronous host-side setup; missing or failing plugin setup
does not prevent normal GUI editing. Install a compatible `nauvim-plugin` on
the host when offering its features. The specification supports the latest core
and plugin together; other combinations have no specified compatibility.

Forwarding, copies, HTTP servers, plugin validation, and their resource handles
are managed by core. Core enforces
[host plugin permissions](usage.md#host-plugin-permissions) when a plugin
requests an operation. The frontend calls core's navigation check for each
top-level target.

### Opening and closing presentations

With `plugin-http`, `OpenWebview` asks the frontend to create a GUI window
asynchronously and load an HTTP(S) URL. Keep the window and its `WebviewWindow`
registration by `ResourceHandle`.

1. Install `WebviewWindow::allows_navigation()` as a check before loading the
   initial URL. Apply it before accessing each top-level navigation target,
   including every redirect. Block the target when the check returns `false`.
   Core checks the opening attachment's policy and reports a denial through
   `EngineEvent::Error`.
2. Before opening the window, enable available developer tools or console
   settings in the presentation component. Start with the tools hidden and
   let the user open them through the component's usual mechanism.
3. Apply `focusable` to the toolkit window. Call `WebviewWindow::opened()` once
   the window exists and can accept navigation. If it returns `false`, dispose
   of the window. Otherwise, focus it immediately if both `focus` and
   `focusable` are true.
4. On `FocusWebview`, find the window by handle and recheck
   `WebviewWindow::can_focus()` before moving focus. If moving focus fails,
   call `WebviewWindow::report_error()` and leave the window open. Use the same
   reporting method for creation or load failures.
5. On `CloseWebview`, close the presentation or cancel outstanding creation
   for that handle. Handle closure idempotently. If `close_on_blur` is true
   and the toolkit reports loss of focus, run the same window cleanup
   immediately. Call `WebviewWindow::closed()` when closing the window or when
   creation fails.

A request for a missing grid returns a handle without emitting `OpenWebview`.
A plugin clear or focus notification has no effect on a window whose creation
is still pending. Attachment cleanup cancels pending creation.

### Navigating an open window

Core checks that the window is open and applies its opening attachment's
origin policy before emitting `NavigateWebview`. When handling the event,
check `WebviewWindow::can_navigate()` and the toolkit's availability. If the
window is unavailable, report a client error through
`WebviewWindow::report_error()` and discard the event. Start allowed navigation
without waiting for the window to become available. Continue to use
`WebviewWindow::allows_navigation()` before accessing each top-level target,
including redirects.

Replace the current history entry if `replace_history` is true. Use the
presentation component's normal history behaviour otherwise. Keep the existing
navigation and history entry when an identical request is already in progress
or still in effect. If `focus` is true, request focus only after the component
accepts navigation and `WebviewWindow::can_focus()` is still true. Report navigation,
history replacement and focus errors through `WebviewWindow::report_error()`. Leave
the window open in the state left by the failed operation.

`WebviewPosition` uses cells of its named grid, with a zero-based upper-left
origin. Increasing `left` moves right; increasing `top` moves down. Convert its
origin and extent through your grid placement, cell metrics, and display scale
into toolkit coordinates. For a uniformly scaled grid this corresponds to:

```text
x      = grid_origin_x + left * cell_width
y      = grid_origin_y + top  * cell_height
width  = requested_width  * cell_width
height = requested_height * cell_height
```

The four position values can be zero. They are validated as integers in
0..=65535, but a range-valid rectangle need not fit the grid; behaviour outside
the grid is undefined by the specification. Grid existence is checked on the
RPC side. An open event can arrive before your UI consumes the frame that
introduces that grid, so defer placement until your adapter has its layout.
The specification leaves general grid arrangement to the frontend; preserve
the requested cell relationship within that arrangement.

Resolve the URL in the client environment through the presentation component.
Preserve fragment identifiers, including when navigating within the same
document. HTTPS presentations must accept the self-signed certificates generated
by core's file-serving interface. The event supplies a URL, not a certificate
or trust-store API. Choose a component and certificate-handling integration that
can meet that requirement. Other PKI behaviour is unspecified by the
specification. File serving supports HTTP/1.1 with and without TLS and HTTP/2
over TLS. Use a presentation component compatible with the URLs your
application offers.

### Resource cleanup

Attachment cleanup cancels its plugin work and emits close events for its
plugin windows. Keep processing those close events even after switching hosts.
If a window closes through a local GUI action, keep subsequent closure of its
handle idempotent.

Core's process temporary directory is exposed by `Engine::temp_dir()` and stays
in place through detachment and host switching. Plugin client paths resolve to
strict descendants of it. They use POSIX path syntax relative to that directory,
with leading slashes removed. Core stops copy writers before removing destinations
owned by those copy operations. A pre-existing destination preserved by
`override = false` remains in place on attachment cleanup. Shutdown removes the
temporary directory and its remaining contents. Do not remove it as part of a
frontend's detach action. Host RPC sockets are persistent session endpoints,
not frontend temporary files.

## Errors, detachment, and shutdown

Keep a client-side error display available during startup, the welcome page,
editor operation, and shutdown. Errors cannot depend on a working host editor
or on a successful WebView navigation to be visible.

| Failure or action | Result to handle |
| --- | --- |
| Configuration load or validation failure | Default configuration plus error events. Invalid clipboard timeout alone uses its narrower fallback. |
| Connection-attempt failure | Returned error and normally a welcome event carrying the error; allow destination selection or retry. |
| `EngineEvent::Error` | Client diagnostic; do not infer an attachment transition from this variant alone. |
| Ordinary `Client` input or API error | Returned error; display it through the application. These errors are not all duplicated as engine error events. |
| Asynchronous Neovim API error | An RPC error in `Frame::events`; handle it even when skipping painting. |
| Backend or plugin-window failure | Display it through the application's error UI. |
| Host exit or detachment | Welcome page and attachment cleanup. |
| Unexpected RPC or session SSH interruption | Welcome page with an error and cleanup of the affected attachment. The application remains running. |

Route host selection, detachment, and shutdown through `Engine`. The inherited
`Client::attach()` and `Client::detach()` methods only perform Neovim UI calls;
they do not run core's attachment-resource lifecycle. Use `Engine::detach()`
for the GUI detach action. It cancels a pending attempt or ends the current
attachment, queues a welcome event, and returns after scheduling cleanup.
It does not wait for all cleanup and does not terminate the host Neovim.

Unexpected-connection recovery does not send a detachment RPC to the host.
The engine retains resources needed for destination selection. Do not terminate
a surviving host process or remove its live socket in the frontend's recovery
or exit path; later application launches reuse owned sessions.

When the last GUI window closes, stop accepting new application actions and
finalize the engine. Include plugin-created GUI windows in the application's
window accounting, using the toolkit's ordinary window-close behaviour.
The following is a **partial finalization example**, with
`ui` providing error display and idempotent window closure:

```rust
use nauvim_core::EngineEvent;

if let Err(error) = engine.shutdown().await {
    ui.show_error(error.to_string());
}

loop {
    match engine.next_event().await {
        EngineEvent::Error(message) => ui.show_error(message),
        EngineEvent::CloseWebview { handle } => ui.close_webview(handle),
        EngineEvent::Shutdown => break,
        _ => {} // Earlier queued status events do not start new work here.
    }
}
```

Keep the runtime and error-display resources alive through that sequence.
Stop or await tasks owned by your frontend as well. Report deletion failures
and proceed to exit without retrying the failed deletion. Dropping the engine
provides best-effort cleanup; it is not a substitute for awaiting `shutdown()`.

Core watches SIGTERM while connecting or receiving events. It performs cleanup
and emits `Shutdown`; your application is responsible for leaving the toolkit
loop and terminating the process. Use the same explicit finalization path when
you receive that event, including if an earlier receive was cancelled during
signal handling. Host Neovim processes remain available for reattachment.

## Frontend validation

Validate the adapter through the public workflow with Neovim v0.12.6 on the
platforms it supports. Useful cases are:

- Automatic default startup, configured startup, selection from the welcome
  page, connection failure, and retry without exiting the application.
- Detachment and application restart followed by reattachment to the same
  surviving host; plugin-requested host switching with old-window cleanup.
- All enabled external widgets, multiple and floating grids, hidden or destroyed
  grids, wide characters, multibyte command-line text, and font or scale changes.
- Deferred painting across several frames, with ordered events delivered once,
  correct removal of stale rendering objects, and repaint after exposure.
- Literal keyboard text containing `<`, input ordering, GUI mouse actions that
  do not generate Neovim mouse input, and focus changes.
- GUI paste of multiline text, unavailable clipboard utilities, paste cancellation,
  and a pending paste during a host switch.
- Plugin-window placement in a secondary grid, a delayed grid frame, fragments,
  generated HTTPS certificates, navigation failure, and cleanup during creation.
- RPC or SSH interruption returning to a usable welcome page, last-window
  shutdown, SIGTERM during connection or event reception, and cleanup error
  display before process exit.

The core's own tests validate engine behaviour, not your toolkit adapter.
See [development and validation](development.md) for those checks and the
requirements of the optional live integration suites.

[spec]: https://github.com/nauvim/nauvim-spec/blob/v1.0.1/specification.md
[ui-crate]: https://docs.rs/nvim-rpc-ui/0.1.3/nvim_rpc_ui/
[ui-protocol]: https://neovim.io/doc/user/api-ui-events/
