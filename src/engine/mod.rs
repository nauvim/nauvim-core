#[cfg(feature = "plugin-http")]
use crate::WebviewWindow;
use crate::connection::Connection;
#[cfg(feature = "ssh")]
use crate::resources::Remote;
#[cfg(feature = "plugin-clipboard")]
use crate::resources::clipboard::Clipboard;
use crate::resources::{Registry, Resources};
use crate::{ClientId, Config, Error, Frame, GridId, ResourceHandle, Transport};

mod attempt;
use attempt::Attempt;

use nvim_rpc_ui::Client as UiClient;

use tempfile::TempDir;
use tokio::io::WriteHalf;
use tokio::runtime::Handle;
use tokio::signal::unix::Signal;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// A cloneable, concrete input client for the current attachment.
pub type Client = UiClient<WriteHalf<Transport>>;
const ATTACH_TIMEOUT: Duration = Duration::from_secs(20);
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// A plugin window's position, measured in cells of its target UI grid.
#[derive(Debug, Clone, Copy)]
pub struct WebviewPosition {
    pub grid: GridId,
    pub top: u16,
    pub left: u16,
    pub height: u16,
    pub width: u16,
}

/// Application work emitted by the engine.
pub enum EngineEvent {
    Frame(Frame),
    Welcome {
        error: Option<String>,
    },
    /// A session switch is starting. Continue receiving events to connect.
    Connect {
        host: String,
    },
    Error(String),
    OpenWebview {
        handle: ResourceHandle,
        url: String,
        position: WebviewPosition,
        /// Whether to focus the window immediately after opening if focusable.
        focus: bool,
        /// Whether the window can receive focus.
        focusable: bool,
        /// The frontend's navigation check and window lifecycle token.
        #[cfg(feature = "plugin-http")]
        window: WebviewWindow,
    },
    /// Focuses an open and focusable plugin window through the frontend.
    FocusWebview {
        handle: ResourceHandle,
    },
    CloseWebview {
        handle: ResourceHandle,
    },
    /// SIGTERM cleanup has completed; the application can terminate.
    Shutdown,
}

struct Attachment {
    rpc: Connection<WriteHalf<Transport>>,
    resources: Arc<Resources>,
    events: mpsc::UnboundedReceiver<EngineEvent>,
    setup: Option<JoinHandle<()>>,
    host_quit: Arc<AtomicBool>,
    host_detach: Arc<AtomicBool>,
    #[cfg(feature = "plugin-clipboard")]
    clipboard: Arc<Clipboard>,
    #[cfg(feature = "ssh")]
    remote: Option<Remote>,
}

impl Attachment {
    async fn cleanup(mut self, detach: bool, events: mpsc::UnboundedSender<EngineEvent>) {
        self.resources.cancel();
        #[cfg(feature = "plugin-clipboard")]
        self.clipboard.cancel();
        #[cfg(feature = "plugin-http")]
        for handle in self.resources.windows().await {
            let _ = events.send(EngineEvent::CloseWebview { handle });
        }
        #[cfg(not(feature = "plugin-http"))]
        let _ = events;
        if let Some(setup) = self.setup.take() {
            setup.abort();
            let _ = setup.await;
        }
        if detach {
            let _ = timeout(Duration::from_secs(2), self.rpc.detach_ui()).await;
        }
        self.rpc.stop();
        self.resources.cleanup().await;
        #[cfg(feature = "plugin-clipboard")]
        if let Err(e) = self.clipboard.shutdown().await {
            let _ = events.send(EngineEvent::Error(e));
        }
        let _ = self.rpc.wait().await;
        #[cfg(feature = "ssh")]
        if let Some(remote) = self.remote.take() {
            use openrussh_client::connect::Disconnect;

            let _ = remote
                .disconnect(Disconnect::ByApplication, "Nauvim detached", "en")
                .await;
        }
    }
}

struct Connecting {
    stop: CancellationToken,
    task: JoinHandle<Result<Attachment, Error>>,
}

/// Manages persistent Neovim sessions and attachment-scoped client resources.
///
/// Consume [`Self::next_event()`] continuously. Dropping an input future or a
/// pending event receive is supported. Use [`Self::shutdown()`] before dropping
/// the engine to wait for resource cleanup; host Neovims remain running.
pub struct Engine {
    config: Arc<Config>,
    client_id: ClientId,
    temp: Option<TempDir>,
    temp_path: PathBuf,
    closed: bool,
    started: bool,
    active: Option<Attachment>,
    connecting: Option<Connecting>,
    cleanup: JoinSet<()>,
    queued: VecDeque<EngineEvent>,
    pending_errors: VecDeque<String>,
    errors_tx: mpsc::UnboundedSender<String>,
    errors: mpsc::UnboundedReceiver<String>,
    cleanup_events_tx: mpsc::UnboundedSender<EngineEvent>,
    cleanup_events: mpsc::UnboundedReceiver<EngineEvent>,
    runtime: Option<Handle>,
    sigterm: Option<Signal>,
    handles: Arc<AtomicI64>,
    registry: Arc<Registry>,
    dimensions: (u32, u32),
    pending_host: Option<String>,
    #[cfg(feature = "plugin-clipboard")]
    clipboard: Arc<Clipboard>,
}

impl Engine {
    /// Creates one process identity and one process temporary directory.
    pub fn new(config: Config) -> Result<Self, Error> {
        use getrandom::fill;
        use tempfile::Builder;

        let mut bytes = [0; 4];
        fill(&mut bytes).map_err(|e| Error::Message(format!("Generate client ID: {e}")))?;
        let temp = Builder::new().prefix("nauvim-").tempdir()?;
        let temp_path = temp.path().to_path_buf();
        let (errors_tx, errors) = mpsc::unbounded_channel();
        let (cleanup_events_tx, cleanup_events) = mpsc::unbounded_channel();
        let pending_errors: VecDeque<_> = config.diagnostics().iter().cloned().collect();
        #[cfg(feature = "plugin-clipboard")]
        let (clipboard, clipboard_errors) = Clipboard::new(config.clipboard());
        #[cfg(feature = "plugin-clipboard")]
        let pending_errors = {
            let mut pending_errors = pending_errors;
            pending_errors.extend(clipboard_errors);
            pending_errors
        };
        Ok(Self {
            config: Arc::new(config),
            client_id: u32::from_ne_bytes(bytes).into(),
            temp: Some(temp),
            temp_path,
            closed: false,
            started: false,
            active: None,
            connecting: None,
            cleanup: JoinSet::new(),
            queued: Default::default(),
            pending_errors,
            errors_tx,
            errors,
            cleanup_events_tx,
            cleanup_events,
            runtime: None,
            sigterm: None,
            handles: Arc::new(AtomicI64::new(0)),
            registry: Arc::default(),
            dimensions: (100, 30),
            pending_host: None,
            #[cfg(feature = "plugin-clipboard")]
            clipboard: Arc::new(clipboard),
        })
    }
    /// Borrows the evaluated configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }
    /// Returns this process's stable unsigned client ID.
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }
    /// Returns the common base directory for plugin client paths.
    pub fn temp_dir(&self) -> &Path {
        self.temp_path.as_path()
    }
    /// Clones the current input client, if attached.
    pub fn client(&self) -> Option<Client> {
        self.active.as_ref().map(|a| a.rpc.client.clone())
    }

    /// Pastes the client system clipboard into the currently attached Neovim.
    ///
    /// Reads the `+` selection and delegates literal text to [`Client::paste()`],
    /// which uses Neovim's paste handler and treats CR, LF and CRLF as line breaks.
    /// Returns `false` when that handler cancels the paste. The client backend
    /// supplies the text directly; provider paste callbacks do not supply it.
    ///
    /// The returned future owns its attachment handles, so it can run alongside
    /// [`Self::next_event()`]. It remains addressed to the attachment selected
    /// when this method was called. Failures are returned and reported through
    /// [`EngineEvent::Error`].
    #[cfg(feature = "plugin-clipboard")]
    pub fn paste(&self) -> impl Future<Output = Result<bool, Error>> + Send + use<> {
        let active = self
            .active
            .as_ref()
            .map(|active| (active.rpc.client.clone(), Arc::clone(&active.clipboard)));
        let errors = self.errors_tx.clone();
        async move {
            let result = async {
                let (client, clipboard) =
                    active.ok_or_else(|| Error::Message("No host Neovim is attached".into()))?;
                let text = clipboard.read("+").await.map_err(Error::Message)?;
                client.paste(&text).await.map_err(Error::from)
            }
            .await;
            if let Err(e) = &result {
                let _ = errors.send(e.to_string());
            }
            result
        }
    }

    /// Resizes the current UI and retains the dimensions for session switching.
    ///
    /// With no attachment, retains the size for the next plugin session switch.
    pub async fn resize(&mut self, width: u32, height: u32) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Message("Engine has shut down".into()));
        }
        if width == 0 || height == 0 {
            return Err(Error::Message("UI dimensions must be positive".into()));
        }
        if let Some(active) = &self.active {
            active.rpc.client.resize(width, height).await?;
        }
        self.dimensions = (width, height);
        Ok(())
    }

    /// Applies startup host selection once, or opens the welcome page.
    ///
    /// Later calls have no effect. Detachment and recovery never repeat the
    /// automatic startup connection.
    pub async fn start(&mut self, width: u32, height: u32) -> Result<(), Error> {
        use crate::HostSelection;

        if self.closed {
            return Err(Error::Message("Engine has shut down".into()));
        }
        if self.started {
            return Ok(());
        }
        self.install_signal()?;
        self.started = true;
        match self.config.startup_host() {
            Some(HostSelection::Default) => self.connect_default(width, height).await,
            Some(HostSelection::Alias(alias)) => {
                let alias = alias.to_owned();
                self.connect(&alias, width, height).await
            }
            None => {
                self.install_signal()?;
                self.queued.push_back(EngineEvent::Welcome { error: None });
                Ok(())
            }
        }
    }

    /// Starts or reuses the owned session for a host alias and attaches its UI.
    ///
    /// On failure there is no active attachment and a welcome event carries the
    /// error. Function-valued paths receive host-role information.
    pub async fn connect(&mut self, alias: &str, width: u32, height: u32) -> Result<(), Error> {
        self.connect_selection(Some(alias), width, height).await
    }

    /// Starts or reuses the independent default local host.
    pub async fn connect_default(&mut self, width: u32, height: u32) -> Result<(), Error> {
        self.connect_selection(None, width, height).await
    }

    async fn connect_selection(
        &mut self,
        alias: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<(), Error> {
        self.begin_connection(alias, width, height)?;
        self.wait_connection().await
    }

    fn attempt(&self) -> Attempt {
        Attempt {
            config: Arc::clone(&self.config),
            client_id: self.client_id,
            temp_path: self.temp_path.clone(),
            errors_tx: self.errors_tx.clone(),
            cleanup_events_tx: self.cleanup_events_tx.clone(),
            handles: Arc::clone(&self.handles),
            registry: Arc::clone(&self.registry),
            active: None,
            #[cfg(feature = "plugin-clipboard")]
            clipboard: Arc::new(self.clipboard.fork()),
        }
    }

    fn begin_connection(
        &mut self,
        alias: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Message("Engine has shut down".into()));
        }
        if let Err(e) = self.install_signal() {
            self.queued.push_back(EngineEvent::Welcome {
                error: Some(e.to_string()),
            });
            return Err(e);
        }
        self.queued.clear();
        self.pending_host = None;
        self.cancel_connection();
        self.release(true);
        if width == 0 || height == 0 {
            let error = Error::Message("UI dimensions must be positive".into());
            self.queued.push_back(EngineEvent::Welcome {
                error: Some(error.to_string()),
            });
            return Err(error);
        }
        self.dimensions = (width, height);
        let attempt = self.attempt();
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let alias = alias.map(str::to_owned);
        // The task owns connection progress independently of event-receive
        // futures; cancelling a receive must not restart startup or lose setup.
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| Error::Message("Connection runtime is unavailable".into()))?;
        let task = runtime.spawn(attempt.run(alias, width, height, task_stop));
        self.connecting = Some(Connecting { stop, task });
        Ok(())
    }

    async fn wait_connection(&mut self) -> Result<(), Error> {
        let Some(connecting) = self.connecting.as_mut() else {
            return Ok(());
        };
        tokio::select! {
            result = &mut connecting.task => self.complete_connection(result),
            () = receive_sigterm(&mut self.sigterm) => {
                let result = self.shutdown().await;
                self.queued.push_back(EngineEvent::Shutdown);
                result.and(Err(Error::Message("Shutdown requested".into())))
            }
        }
    }

    fn complete_connection(
        &mut self,
        result: Result<Result<Attachment, Error>, JoinError>,
    ) -> Result<(), Error> {
        self.connecting = None;
        let result = result.map_err(Error::from).and_then(|result| result);
        match result {
            Ok(active) => {
                #[cfg(feature = "plugin-clipboard")]
                {
                    self.clipboard = Arc::clone(&active.clipboard);
                }
                self.active = Some(active);
                Ok(())
            }
            Err(e) => {
                self.queued.push_back(EngineEvent::Welcome {
                    error: Some(e.to_string()),
                });
                Err(e)
            }
        }
    }

    fn cancel_connection(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        if let Some(connecting) = self.connecting.take() {
            connecting.stop.cancel();
            let events = self.cleanup_events_tx.clone();
            self.cleanup.spawn_on(
                async move {
                    if let Ok(Ok(active)) = connecting.task.await {
                        active.cleanup(false, events).await;
                    }
                },
                runtime,
            );
        }
    }

    fn release(&mut self, detach: bool) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        while self.cleanup.try_join_next().is_some() {}
        if let Some(active) = self.active.take() {
            active.resources.cancel();
            #[cfg(feature = "plugin-clipboard")]
            active.clipboard.cancel();
            self.cleanup.spawn_on(
                active.cleanup(detach, self.cleanup_events_tx.clone()),
                runtime,
            );
        }
    }

    /// Ends the attachment and returns immediately after scheduling its cleanup.
    ///
    /// Cleanup stays associated with the old attachment during session switches.
    pub async fn detach(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.ensure_runtime()?;
        self.pending_host = None;
        self.cancel_connection();
        self.release(true);
        self.queued.clear();
        self.queued.push_back(EngineEvent::Welcome { error: None });
        Ok(())
    }

    /// Receives a frame, plugin GUI operation, or connection lifecycle event.
    ///
    /// Cancelling this wait preserves pending events. A connection failure closes
    /// local resources without sending a detachment RPC to the host.
    pub async fn next_event(&mut self) -> EngineEvent {
        use nvim_rpc_plugin::ConnectionError;

        if let Some(event) = self.pending_event() {
            return event;
        }
        if self.closed {
            return EngineEvent::Shutdown;
        }
        if let Some(host) = self.pending_host.take() {
            let (width, height) = self.dimensions;
            let _ = self.begin_connection(Some(&host), width, height);
            if let Some(event) = self.queued.pop_front() {
                return event;
            }
        }
        if let Err(e) = self.install_signal() {
            return EngineEvent::Error(e.to_string());
        }
        loop {
            if let Some(connecting) = self.connecting.as_mut() {
                tokio::select! {
                    Some(error) = self.errors.recv() => return EngineEvent::Error(error),
                    Some(event) = self.cleanup_events.recv() => return event,
                    () = receive_sigterm(&mut self.sigterm) => return self.shutdown_event().await,
                    result = &mut connecting.task => {
                        let _ = self.complete_connection(result);
                        if let Some(event) = self.pending_event() {
                            return event;
                        }
                    }
                }
            }
            let Some(active) = self.active.as_mut() else {
                return tokio::select! {
                    Some(error) = self.errors.recv() => EngineEvent::Error(error),
                    Some(event) = self.cleanup_events.recv() => event,
                    () = receive_sigterm(&mut self.sigterm) => self.shutdown_event().await,
                };
            };
            return tokio::select! {
                Some(error) = self.errors.recv() => EngineEvent::Error(error),
                Some(event) = self.cleanup_events.recv() => event,
                () = receive_sigterm(&mut self.sigterm) => self.shutdown_event().await,
                event = active.events.recv(), if !active.events.is_closed() || !active.events.is_empty() => {
                    match event {
                        Some(EngineEvent::Connect {host}) => {
                            self.release(false);
                            self.pending_host = Some(host.clone());
                            self.queued.push_back(EngineEvent::Connect {host});
                            EngineEvent::Welcome {error:None}
                        }
                        Some(event) => event,
                        None => continue,
                    }
                }
                frame = active.rpc.frames.recv() => {
                    match frame {
                        Some(Ok(frame)) => {
                            EngineEvent::Frame(frame)
                        }
                        Some(Err(e)) => {
                            self.departure(Some(e.to_string()))
                        }
                        None => {
                            let error = match active.rpc.wait().await {
                                Ok(()) | Err(ConnectionError::Eof { buffered: 0 }) => {
                                    #[cfg(feature = "ssh")]
                                    let ssh_closed = active.remote.as_ref().is_some_and(|r| r.handle().is_closed());
                                    #[cfg(not(feature = "ssh"))]
                                    let ssh_closed = false;
                                    if active.host_quit.load(Ordering::Acquire) || active.host_detach.load(Ordering::Acquire) { None }
                                else if ssh_closed { Some("SSH connection was interrupted".into()) }
                                else { Some("Host Neovim connection was interrupted".into()) }
                            },
                                Err(e) => Some(e.to_string()),
                            };
                            self.departure(error)
                        }
                    }
                }
            };
        }
    }

    fn departure(&mut self, error: Option<String>) -> EngineEvent {
        let mut host = None;
        if let Some(active) = self.active.as_mut() {
            // A connection can close after its switch notification was accepted.
            // Drain that queue before releasing its receiver, regardless of which
            // ready select branch observed termination first.
            while let Ok(event) = active.events.try_recv() {
                match event {
                    EngineEvent::Connect { host: destination } => host = Some(destination),
                    EngineEvent::OpenWebview { .. } | EngineEvent::FocusWebview { .. } => {}
                    event => self.queued.push_back(event),
                }
            }
        }
        self.release(false);
        if let Some(host) = host {
            self.pending_host = Some(host.clone());
            self.queued.push_back(EngineEvent::Connect { host });
        }
        EngineEvent::Welcome { error }
    }

    /// Stops resources and connections, preserving every host Neovim process.
    pub async fn shutdown(&mut self) -> Result<(), Error> {
        self.ensure_runtime()?;
        self.closed = true;
        self.pending_host = None;
        self.cancel_connection();
        self.release(true);
        while let Some(result) = self.cleanup.join_next().await {
            if let Err(e) = result {
                self.pending_errors
                    .push_back(format!("Attachment cleanup failed: {e}"));
            }
        }
        #[cfg(feature = "plugin-clipboard")]
        let clipboard = self.clipboard.shutdown().await.map_err(Error::Message);
        let temp = self.temp.take().map_or(Ok(()), TempDir::close);
        while let Ok(error) = self.errors.try_recv() {
            self.pending_errors.push_back(error);
        }
        while let Ok(event) = self.cleanup_events.try_recv() {
            self.queued.push_back(event);
        }
        #[cfg(feature = "plugin-clipboard")]
        clipboard?;
        temp?;

        Ok(())
    }

    fn install_signal(&mut self) -> Result<(), Error> {
        use tokio::signal::unix::{SignalKind, signal};

        self.ensure_runtime()?;
        if self.sigterm.is_none() {
            let runtime = self
                .runtime
                .as_ref()
                .ok_or_else(|| Error::Message("Signal runtime is unavailable".into()))?;
            let _entered = runtime.enter();
            self.sigterm = Some(signal(SignalKind::terminate())?);
        }
        Ok(())
    }

    fn ensure_runtime(&mut self) -> Result<(), Error> {
        if self.runtime.is_none() {
            self.runtime = Some(
                Handle::try_current()
                    .map_err(|e| Error::Message(format!("Start Nauvim engine tasks: {e}")))?,
            );
        }
        Ok(())
    }

    fn pending_event(&mut self) -> Option<EngineEvent> {
        self.pending_errors
            .pop_front()
            .map(EngineEvent::Error)
            .or_else(|| self.queued.pop_front())
    }

    async fn shutdown_event(&mut self) -> EngineEvent {
        if let Err(e) = self.shutdown().await {
            self.pending_errors.push_back(e.to_string());
        }
        self.queued.push_back(EngineEvent::Shutdown);
        self.pending_event().unwrap_or(EngineEvent::Shutdown)
    }
}

async fn receive_sigterm(signal: &mut Option<Signal>) {
    use std::future::pending;

    if let Some(signal) = signal {
        signal.recv().await;
    } else {
        pending().await
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        use std::mem;

        let connecting = self.connecting.take();
        if let Some(connecting) = &connecting {
            connecting.stop.cancel();
        }
        let active = self.active.take();
        let mut cleanup = mem::take(&mut self.cleanup);
        let temp = self.temp.take();
        let events = self.cleanup_events_tx.clone();
        #[cfg(feature = "plugin-clipboard")]
        let clipboard = Arc::clone(&self.clipboard);
        let runtime = self.runtime.take().or_else(|| Handle::try_current().ok());
        if let Some(runtime) = runtime {
            runtime.spawn(async move {
                if let Some(connecting) = connecting
                    && let Ok(Ok(active)) = connecting.task.await
                {
                    active.cleanup(false, events.clone()).await;
                }
                if let Some(active) = active {
                    active.cleanup(false, events).await;
                }
                while cleanup.join_next().await.is_some() {}
                #[cfg(feature = "plugin-clipboard")]
                let _ = clipboard.shutdown().await;
                drop(temp);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::connection;
    #[cfg(feature = "plugin-http")]
    use crate::connection::Grids;

    use super::*;

    use nvim_rpc_ui::{ConnectionOptions, NoHandler};

    use tokio::io::split;
    use tokio::net::UnixStream;
    use tokio::spawn;

    #[cfg(feature = "plugin-clipboard")]
    #[tokio::test]
    async fn paste_without_attachment_owns_its_future_and_reports_the_error() {
        let mut engine = Engine::new(Config::default()).unwrap();
        engine.pending_errors.clear();
        let paste = spawn(engine.paste());
        assert_eq!(
            paste.await.unwrap().unwrap_err().to_string(),
            "No host Neovim is attached"
        );
        let EngineEvent::Error(error) = timeout(Duration::from_secs(1), engine.next_event())
            .await
            .unwrap()
        else {
            panic!("Expected a paste error event");
        };
        assert_eq!(error, "No host Neovim is attached");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retains_connection_progress_when_event_receive_is_cancelled() {
        use tokio::sync::Notify;

        use std::future::{Future as _, poll_fn};
        use std::sync::atomic::AtomicUsize;
        use std::task::Poll;

        let mut engine = Engine::new(Config::default()).unwrap();
        engine.pending_errors.clear();
        let entered = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let starts = Arc::new(AtomicUsize::new(0));
        let task = {
            let entered = Arc::clone(&entered);
            let resume = Arc::clone(&resume);
            let starts = Arc::clone(&starts);
            spawn(async move {
                starts.fetch_add(1, Ordering::Relaxed);
                entered.notify_one();
                resume.notified().await;
                Err(Error::Message("Test attachment completed".into()))
            })
        };
        engine.connecting = Some(Connecting {
            stop: CancellationToken::new(),
            task,
        });

        for _ in 0..3 {
            let mut event = Box::pin(engine.next_event());
            poll_fn(|cx| {
                assert!(event.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(event);
            assert!(engine.connecting.is_some());
        }
        entered.notified().await;
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        resume.notify_one();
        let event = timeout(Duration::from_secs(2), engine.next_event())
            .await
            .unwrap();
        let EngineEvent::Welcome { error } = event else {
            panic!("Expected the completed attempt's welcome event");
        };
        assert_eq!(error.as_deref(), Some("Test attachment completed"));
        assert!(engine.connecting.is_none());
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_signals_and_collects_a_pending_connection() {
        use tokio::sync::oneshot;

        let mut engine = Engine::new(Config::default()).unwrap();
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let (stopped, stopped_rx) = oneshot::channel();
        let task = spawn(async move {
            task_stop.cancelled().await;
            let _ = stopped.send(());
            Err(Error::Message("Stopped test attachment".into()))
        });
        engine.connecting = Some(Connecting { stop, task });
        timeout(Duration::from_secs(2), engine.shutdown())
            .await
            .unwrap()
            .unwrap();
        stopped_rx.await.unwrap();
        assert!(engine.connecting.is_none());
        assert!(!engine.temp_dir().exists());
    }

    #[tokio::test]
    async fn retains_accepted_switch_when_frame_delivery_terminates() {
        let mut engine = Engine::new(Config::default()).unwrap();
        engine.pending_errors.clear();
        engine.install_signal().unwrap();
        let (local, _peer) = UnixStream::pair().unwrap();
        let (rdr, wtr) = split(Transport::local(local));
        let rpc = connection::connect(
            rdr,
            wtr,
            NoHandler,
            ConnectionOptions::default(),
            #[cfg(feature = "plugin-http")]
            Grids::default(),
        )
        .unwrap();
        let (events_tx, events) = mpsc::unbounded_channel();
        events_tx
            .send(EngineEvent::Connect {
                host: "next-host".into(),
            })
            .unwrap_or_else(|_| panic!("Attachment event receiver disappeared"));
        engine.active = Some(Attachment {
            rpc,
            resources: Arc::new(Resources::new(
                engine.temp_path.clone(),
                engine.errors_tx.clone(),
            )),
            events,
            setup: None,
            host_quit: Arc::new(AtomicBool::new(false)),
            host_detach: Arc::new(AtomicBool::new(true)),
            #[cfg(feature = "plugin-clipboard")]
            clipboard: Arc::clone(&engine.clipboard),
            #[cfg(feature = "ssh")]
            remote: None,
        });

        assert!(matches!(
            engine.departure(None),
            EngineEvent::Welcome { error: None }
        ));
        assert_eq!(engine.pending_host.as_deref(), Some("next-host"));
        let EngineEvent::Connect { host } = engine.next_event().await else {
            panic!("The accepted switch destination was lost");
        };
        assert_eq!(host, "next-host");
        assert!(engine.active.is_none());
        engine.shutdown().await.unwrap();
    }

    #[test]
    fn schedules_cleanup_on_the_stored_runtime_outside_its_context() {
        use tokio::runtime::Builder;

        use std::future::Future as _;
        use std::task::{Context, Poll, Waker};

        let mut engine = Engine::new(Config::default()).unwrap();
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async { engine.ensure_runtime().unwrap() });
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let task = runtime.spawn(async move {
            task_stop.cancelled().await;
            Err(Error::Message("Stopped test attachment".into()))
        });
        engine.connecting = Some(Connecting { stop, task });

        {
            let mut detach = std::pin::pin!(engine.detach());
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(detach.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }
        assert!(engine.connecting.is_none());
        runtime.block_on(engine.shutdown()).unwrap();
        assert!(!engine.temp_dir().exists());
    }

    #[tokio::test]
    async fn client_registration_failure_keeps_ui_attached_and_skips_plugin_setup() {
        use nvim_rpc_plugin::mp::encode::primitive::Map;
        use nvim_rpc_plugin::mp::encode::{Untagged, primitive::Nil};
        use nvim_rpc_plugin::{
            Array, ConnectionOptions as RpcOptions, Handler, Neovim, Object, api,
            connect as connect_rpc,
        };

        use tokio::io::AsyncWrite;

        use std::sync::Mutex;

        type Calls = Arc<Mutex<Vec<(String, Option<String>)>>>;
        struct Host(Calls);

        impl<W: AsyncWrite + Unpin + Send + 'static> Handler<W> for Host {
            type Request = Array;
            type Notification = Array;
            type Response =
                Result<Untagged<Nil, (i64, Map<[(&'static str, i64); 0]>)>, &'static str>;

            async fn on_request(
                &self,
                method: &[u8],
                mut params: Array,
                _: Neovim<W>,
            ) -> Self::Response {
                use nvim_rpc_plugin::{Bytes, Decode, sys::types};
                let source = if method == b"nvim_exec_lua" {
                    params.next().map(|value| {
                        <String as Decode<types::Object, Bytes>>::decode(value).unwrap()
                    })
                } else {
                    None
                };
                self.0
                    .lock()
                    .unwrap()
                    .push((String::from_utf8(method.to_vec()).unwrap(), source));
                match method {
                    b"nvim_set_client_info" => Err("Registration deliberately rejected"),
                    b"nvim_get_api_info" => Ok(Untagged::Right((7, Map([])))),
                    _ => Ok(Untagged::Left(Nil)),
                }
            }

            async fn on_notification(&self, _: &[u8], _: Array, _: Neovim<W>) {}
        }

        timeout(Duration::from_secs(2), async {
            let mut engine = Engine::new(Config::default()).unwrap();
            engine.pending_errors.clear();
            let calls: Calls = Arc::default();
            let (local, remote) = UnixStream::pair().unwrap();
            let (rdr, wtr) = split(remote);
            let mut host =
                connect_rpc(rdr, wtr, Host(Arc::clone(&calls)), RpcOptions::default()).unwrap();
            let (rdr, wtr) = split(Transport::local(local));
            let rpc = connection::connect(
                rdr,
                wtr,
                NoHandler,
                ConnectionOptions::default(),
                #[cfg(feature = "plugin-http")]
                Grids::default(),
            )
            .unwrap();
            let (_, events) = mpsc::unbounded_channel();
            engine.active = Some(Attachment {
                rpc,
                resources: Arc::new(Resources::new(
                    engine.temp_path.clone(),
                    engine.errors_tx.clone(),
                )),
                events,
                setup: None,
                host_quit: Arc::new(AtomicBool::new(false)),
                host_detach: Arc::new(AtomicBool::new(false)),
                #[cfg(feature = "plugin-clipboard")]
                clipboard: Arc::clone(&engine.clipboard),
                #[cfg(feature = "ssh")]
                remote: None,
            });

            let mut attempt = engine.attempt();
            attempt.active = engine.active.take();
            attempt.register(80, 24).await.unwrap();
            engine.active = attempt.active.take();
            assert!(engine.active.as_ref().unwrap().setup.is_none());
            assert!(engine.pending_errors.is_empty());
            engine
                .client()
                .unwrap()
                .nvim()
                .request::<api::NvimUiTryResize, Object>((100i64, 30i64))
                .await
                .unwrap();
            {
                let calls = calls.lock().unwrap();
                assert!(calls.iter().any(|(method, _)| method == "nvim_ui_attach"));
                assert!(
                    calls
                        .iter()
                        .any(|(method, _)| method == "nvim_ui_try_resize")
                );
                assert!(calls.iter().all(|(_, source)| {
                    source
                        .as_ref()
                        .is_none_or(|source| !source.contains("nauvim-plugin.client"))
                }));
            }
            engine.shutdown().await.unwrap();
            host.close_and_wait().await.unwrap();
        })
        .await
        .expect("Registration failure test timed out");
    }

    #[tokio::test]
    async fn emits_ui_schema_failure_after_resetting_configuration_to_defaults() {
        use crate::{HostSelection, UiValue};

        use tempfile::tempdir;

        use std::fs::write;

        let root = tempdir().unwrap();
        let file = root.path().join("init.lua");
        write(
            &file,
            "return {hosts={custom={}},ui_opts={test={size='invalid'}}}",
        )
        .unwrap();
        let (config, size) = Config::load(Some(&file), "test", |opts| match opts.get("size") {
            None => Ok(12),
            Some(UiValue::Integer(n)) => Ok(*n),
            _ => Err(Error::Message("size must be an integer".into())),
        })
        .unwrap();
        assert_eq!(size, 12);
        assert!(config.hosts().next().is_none());
        assert_eq!(config.startup_host(), Some(HostSelection::Default));
        let diagnostic = config.diagnostics().first().unwrap().clone();
        assert!(diagnostic.contains("UI options"), "{diagnostic}");
        let mut engine = Engine::new(config).unwrap();
        let EngineEvent::Error(error) = engine.next_event().await else {
            panic!("Expected the selected UI schema diagnostic");
        };
        assert_eq!(error, diagnostic);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn preserves_nonfatal_clipboard_diagnostics_across_connection_attempts() {
        use tempfile::tempdir;

        use std::fs::write;

        let root = tempdir().unwrap();
        let file = root.path().join("init.lua");
        write(
            &file,
            "return {clipboard={backend='missing-test-backend',timeout_ms=0}}",
        )
        .unwrap();
        let (config, ()) =
            Config::load(Some(&file), "org.example.nauvim.engine-test", |_| Ok(())).unwrap();
        assert_eq!(config.diagnostics().len(), 1);
        let timeout_error = config.diagnostics().first().unwrap().clone();
        let mut engine = Engine::new(config).unwrap();
        assert!(engine.connect("local", 0, 20).await.is_err());
        let EngineEvent::Error(error) = engine.next_event().await else {
            panic!("Expected the nonfatal timeout diagnostic");
        };
        assert_eq!(error, timeout_error);
        #[cfg(feature = "plugin-clipboard")]
        {
            let EngineEvent::Error(error) = engine.next_event().await else {
                panic!("Expected the unavailable backend diagnostic");
            };
            assert!(error.contains("missing-test-backend"));
        }
        assert!(engine.client().is_none());
        engine.shutdown().await.unwrap();
    }
}
