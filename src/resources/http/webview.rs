use crate::config::WebviewPolicy;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Default)]
pub(in crate::resources) struct ServedOrigins {
    active: RwLock<BTreeMap<(bool, u16), CancellationToken>>,
}

impl ServedOrigins {
    pub(super) fn register(
        self: &Arc<Self>,
        https: bool,
        port: u16,
        stop: CancellationToken,
    ) -> ActiveOrigin {
        let key = (https, port);
        self.active
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, stop);

        ActiveOrigin {
            origins: Arc::clone(self),
            key,
        }
    }

    fn contains(&self, url: &Url) -> bool {
        let https = match url.scheme() {
            "http" => false,
            "https" => true,
            _ => return false,
        };
        let Some(port) = url.port_or_known_default() else {
            return false;
        };

        url.host() == Some(Host::Ipv4(Ipv4Addr::LOCALHOST))
            && self
                .active
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(https, port))
                .is_some_and(|stop| !stop.is_cancelled())
    }
}

pub(super) struct ActiveOrigin {
    origins: Arc<ServedOrigins>,
    key: (bool, u16),
}

impl Drop for ActiveOrigin {
    fn drop(&mut self) {
        self.origins
            .active
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

pub(crate) struct WebviewNavigation {
    policy: WebviewPolicy,
    served: Arc<ServedOrigins>,
}

impl WebviewNavigation {
    pub(super) fn new(policy: WebviewPolicy, served: Arc<ServedOrigins>) -> Self {
        Self { policy, served }
    }

    pub(crate) fn allows_url(&self, url: &Url) -> bool {
        self.policy.allows_origin(url)
            || (self.policy.permits_served_files() && self.served.contains(url))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Open,
    Closed,
}

pub(in crate::resources) struct Window {
    navigation: WebviewNavigation,
    stop: CancellationToken,
    errors: mpsc::UnboundedSender<String>,
    focusable: bool,
    state: Mutex<State>,
}

impl Window {
    pub(super) fn is_associated(&self) -> bool {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) != State::Closed
    }

    pub(super) fn can_focus(&self) -> bool {
        self.focusable
            && *self.state.lock().unwrap_or_else(|e| e.into_inner()) == State::Open
            && !self.stop.is_cancelled()
    }

    pub(super) fn close_open(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if *state != State::Open {
            return false;
        }

        *state = State::Closed;
        true
    }
}

/// Tracks a frontend window and checks navigation against its attachment's policy.
#[derive(Clone)]
pub struct WebviewWindow(pub(super) Arc<Window>);

impl WebviewWindow {
    pub(super) fn new(
        navigation: WebviewNavigation,
        stop: CancellationToken,
        errors: mpsc::UnboundedSender<String>,
        focusable: bool,
    ) -> Self {
        Self(Arc::new(Window {
            navigation,
            stop,
            errors,
            focusable,
            state: Mutex::new(State::Pending),
        }))
    }

    /// Checks a top-level navigation target before the frontend starts loading it.
    ///
    /// Check the initial URL and each redirect. Denied targets produce an engine
    /// error event and must be blocked by the frontend.
    pub fn allows_navigation(&self, url: &str) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if *state == State::Closed || self.0.stop.is_cancelled() {
            return false;
        }
        if Url::parse(url).is_ok_and(|url| self.0.navigation.allows_url(&url)) {
            return true;
        }

        self.report_error(format!(
            "Client policy denies Webview navigation to {url:?}"
        ));
        false
    }

    /// Marks a constructed window as open and eligible for focus notifications.
    ///
    /// Retain a clone for as long as the window is open. If this returns `false`,
    /// discard the window because its attachment or operation has closed.
    pub fn opened(&self) -> bool {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if *state == State::Closed || self.0.stop.is_cancelled() {
            return false;
        }

        *state = State::Open;
        true
    }

    /// Marks the window as closed or its creation as failed.
    pub fn closed(&self) {
        *self.0.state.lock().unwrap_or_else(|e| e.into_inner()) = State::Closed;
    }

    /// Checks whether the window is still open and can receive focus.
    pub fn can_focus(&self) -> bool {
        self.0.can_focus()
    }

    /// Reports a frontend creation, loading or focus error as an engine event.
    pub fn report_error(&self, error: impl Into<String>) {
        let _ = self.0.errors.send(error.into());
    }
}
