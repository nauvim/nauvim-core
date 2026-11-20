//! Attachment-scoped plugin resources and cancellation.

#[cfg(feature = "plugin-clipboard")]
pub mod clipboard;
#[cfg(feature = "plugin-filesystem")]
mod copy;
#[cfg(feature = "plugin-local-forward")]
mod forward;
#[cfg(feature = "plugin-http")]
mod http;
mod path;

#[cfg(feature = "ssh")]
use openrussh_client::connect::Connection;
#[cfg(feature = "ssh")]
use openrussh_client::known_hosts::KnownHostsHandler;

#[cfg(any(feature = "plugin-local-forward", feature = "plugin-http"))]
use tokio::sync::watch;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use std::collections::BTreeMap;
use std::fmt;
#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(any(feature = "plugin-filesystem", feature = "plugin-local-forward"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicI64, Ordering};

/// An opaque integer identifying a plugin operation or client resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceHandle(i64);

impl ResourceHandle {
    /// Returns the integer used in the plugin RPC protocol.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for ResourceHandle {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl From<ResourceHandle> for i64 {
    fn from(value: ResourceHandle) -> Self {
        value.0
    }
}

impl fmt::Display for ResourceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(feature = "ssh")]
pub type Remote = Arc<Connection<KnownHostsHandler>>;

#[derive(Clone, Debug)]
#[cfg(any(feature = "plugin-local-forward", feature = "plugin-http"))]
enum Binding {
    Pending,
    Ready(u16),
    Failed(String),
}

#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Reserved,
    #[cfg(feature = "plugin-local-forward")]
    Tcp,
    #[cfg(feature = "plugin-local-forward")]
    Socket,
    #[cfg(feature = "plugin-filesystem")]
    Copy,
    #[cfg(feature = "plugin-http")]
    Http,
}

enum Data {
    Reserved,
    #[cfg(feature = "plugin-local-forward")]
    Tcp(watch::Receiver<Binding>),
    #[cfg(feature = "plugin-local-forward")]
    Socket {
        path: PathBuf,
        bound: Arc<AtomicBool>,
    },
    #[cfg(feature = "plugin-filesystem")]
    Copy {
        path: PathBuf,
        preserve: Arc<AtomicBool>,
    },
    #[cfg(feature = "plugin-http")]
    Http {
        path: PathBuf,
        binding: watch::Receiver<Binding>,
    },
}

impl Data {
    #[cfg(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http"
    ))]
    fn kind(&self) -> Kind {
        match self {
            Self::Reserved => Kind::Reserved,
            #[cfg(feature = "plugin-local-forward")]
            Self::Tcp(_) => Kind::Tcp,
            #[cfg(feature = "plugin-local-forward")]
            Self::Socket { .. } => Kind::Socket,
            #[cfg(feature = "plugin-filesystem")]
            Self::Copy { .. } => Kind::Copy,
            #[cfg(feature = "plugin-http")]
            Self::Http { .. } => Kind::Http,
        }
    }

    #[cfg(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http"
    ))]
    fn path(&self) -> Option<&Path> {
        match self {
            #[cfg(feature = "plugin-local-forward")]
            Self::Socket { path, .. } => Some(path),
            #[cfg(feature = "plugin-filesystem")]
            Self::Copy { path, .. } => Some(path),
            #[cfg(feature = "plugin-http")]
            Self::Http { path, .. } => Some(path),
            _ => None,
        }
    }

    #[cfg(any(feature = "plugin-local-forward", feature = "plugin-http"))]
    fn binding(&self) -> Option<&watch::Receiver<Binding>> {
        match self {
            #[cfg(feature = "plugin-local-forward")]
            Self::Tcp(binding) => Some(binding),
            #[cfg(feature = "plugin-http")]
            Self::Http { binding, .. } => Some(binding),
            _ => None,
        }
    }
}

struct Resource {
    data: Data,
    owner: Arc<()>,
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Resource {
    fn new(data: Data, stop: &CancellationToken, owner: &Arc<()>) -> Self {
        Self {
            data,
            owner: owner.clone(),
            stop: stop.child_token(),
            task: None,
        }
    }
}

#[derive(Default)]
struct State {
    entries: BTreeMap<ResourceHandle, Resource>,
}

#[derive(Default)]
pub(crate) struct Registry {
    state: Mutex<State>,
    #[cfg(feature = "plugin-filesystem")]
    directories: Mutex<copy::Directories>,
}

/// Owns the resources created by one attachment.
///
/// Handles are unique across modules. Call [`Self::cleanup()`] before releasing
/// the attachment: it signals all tasks before waiting and stops every copy
/// worker before deleting any destination it owns.
pub struct Resources {
    base: PathBuf,
    errors: mpsc::UnboundedSender<String>,
    registry: Arc<Registry>,
    owner: Arc<()>,
    stop: CancellationToken,
    handles: Arc<AtomicI64>,
    #[cfg(feature = "ssh")]
    remote: Option<Remote>,
}

impl Resources {
    /// Creates resource storage rooted at the process temporary directory.
    pub fn new(base: PathBuf, errors: mpsc::UnboundedSender<String>) -> Self {
        Self::with_counter(base, errors, Arc::new(AtomicI64::new(0)))
    }

    pub(crate) fn with_counter(
        base: PathBuf,
        errors: mpsc::UnboundedSender<String>,
        handles: Arc<AtomicI64>,
    ) -> Self {
        Self::with_registry(base, errors, handles, Arc::new(Registry::default()))
    }

    pub(crate) fn with_registry(
        base: PathBuf,
        errors: mpsc::UnboundedSender<String>,
        handles: Arc<AtomicI64>,
        registry: Arc<Registry>,
    ) -> Self {
        Self {
            base,
            errors,
            registry,
            owner: Arc::new(()),
            stop: CancellationToken::new(),
            handles,
            #[cfg(feature = "ssh")]
            remote: None,
        }
    }

    /// Uses the attachment's SSH connection for forwarding and source access.
    #[cfg(feature = "ssh")]
    pub fn set_remote(&mut self, remote: Remote) {
        self.remote = Some(remote);
    }

    /// Reserves a handle in the same namespace as plugin resources.
    pub async fn reserve(&self) -> Result<ResourceHandle, String> {
        self.insert(Resource::new(Data::Reserved, &self.stop, &self.owner))
            .await
    }

    async fn insert(&self, resource: Resource) -> Result<ResourceHandle, String> {
        let mut state = self.registry.state.lock().await;
        if self.stop.is_cancelled() {
            drop(state);
            self.stop(resource).await;
            return Err("Attachment resources are closed".to_owned());
        }
        let mut previous = self.handles.load(Ordering::Relaxed);
        let next = loop {
            let Some(next) = previous.checked_add(1) else {
                break None;
            };
            match self.handles.compare_exchange_weak(
                previous,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break Some(next),
                Err(current) => previous = current,
            }
        };
        let Some(next) = next else {
            drop(state);
            self.stop(resource).await;
            return Err("Resource handles exhausted".to_owned());
        };
        let handle = ResourceHandle(next);
        state.entries.insert(handle, resource);
        Ok(handle)
    }

    #[cfg(any(feature = "plugin-local-forward", feature = "plugin-http"))]
    async fn port(&self, handle: ResourceHandle, kind: Kind) -> Result<Option<u16>, String> {
        let (mut binding, stop) = {
            let state = self.registry.state.lock().await;
            let resource = state
                .entries
                .get(&handle)
                .filter(|resource| resource.data.kind() == kind)
                .ok_or_else(|| "Unknown resource handle or incorrect resource type".to_owned())?;
            let binding = resource
                .data
                .binding()
                .cloned()
                .ok_or_else(|| "Resource has no TCP binding".to_owned())?;
            (binding, resource.stop.clone())
        };
        loop {
            if stop.is_cancelled() {
                return Ok(None);
            }
            match binding.borrow().clone() {
                Binding::Pending => {}
                Binding::Ready(port) => return Ok(Some(port)),
                Binding::Failed(e) => return Err(e),
            }
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(None),
                ret = binding.changed() => {
                    ret.map_err(|_| "Binding task ended without an outcome".to_owned())?;
                }
            }
        }
    }

    #[cfg(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http"
    ))]
    async fn take(&self, handle: ResourceHandle, kinds: &[Kind]) -> Option<Resource> {
        let mut state = self.registry.state.lock().await;
        if state.entries.get(&handle).is_some_and(|resource| {
            kinds.contains(&resource.data.kind())
                && resource
                    .data
                    .path()
                    .is_none_or(|target| path::descendant(&self.base, target).is_ok())
        }) {
            state.entries.remove(&handle)
        } else {
            None
        }
    }

    async fn stop(&self, mut resource: Resource) {
        self.wait_task(&mut resource).await;
        self.delete(resource).await;
        #[cfg(feature = "plugin-filesystem")]
        self.prune_dirs().await;
    }

    async fn wait_task(&self, resource: &mut Resource) {
        resource.stop.cancel();
        if let Some(task) = resource.task.take()
            && let Err(e) = task.await
        {
            self.report(format!("Resource task failed: {e}"));
        }
    }

    async fn delete(&self, resource: Resource) {
        let data = resource.data;
        #[cfg(any(feature = "plugin-local-forward", feature = "plugin-filesystem"))]
        match data {
            #[cfg(feature = "plugin-local-forward")]
            Data::Socket { path, bound }
                if bound.swap(false, Ordering::AcqRel)
                    && path::descendant(&self.base, &path).is_ok() =>
            {
                use tokio::fs::remove_file;

                use std::io::ErrorKind;

                if let Err(e) = remove_file(&path).await
                    && e.kind() != ErrorKind::NotFound
                {
                    self.report(format!(
                        "Remove local forwarding socket {}: {e}",
                        path.display()
                    ));
                }
            }
            #[cfg(feature = "plugin-filesystem")]
            Data::Copy { path, preserve }
                if path::descendant(&self.base, &path).is_ok()
                    && !preserve.load(Ordering::Acquire) =>
            {
                if let Err(e) = copy::delete_destination(&self.base, &path).await {
                    self.report(format!("Remove copy destination {}: {e}", path.display()));
                }
            }
            _ => {}
        }
        #[cfg(not(any(feature = "plugin-local-forward", feature = "plugin-filesystem")))]
        let _ = data;
    }

    /// Signals every resource task, including work that has not started.
    pub fn cancel(&self) {
        self.stop.cancel();
    }

    /// Stops attachment tasks and removes their owned copy destinations.
    pub async fn cleanup(&self) {
        self.cancel();
        let mut entries = {
            let mut state = self.registry.state.lock().await;
            state
                .entries
                .extract_if(.., |_, resource| Arc::ptr_eq(&resource.owner, &self.owner))
                .map(|(_, resource)| resource)
                .collect::<Vec<_>>()
        };
        for resource in &entries {
            resource.stop.cancel();
        }
        for resource in &mut entries {
            self.wait_task(resource).await;
        }
        for resource in entries {
            self.delete(resource).await;
        }
        #[cfg(feature = "plugin-filesystem")]
        self.prune_dirs().await;
    }

    fn report(&self, e: String) {
        let _ = self.errors.send(e);
    }

    /// Resolves a plugin POSIX path to a strict descendant of the process directory.
    ///
    /// Leading slashes are ignored. Symbolic links and relative components are
    /// resolved in filesystem order; missing descendant entries are permitted.
    pub fn client_path(&self, path: &str) -> Result<PathBuf, String> {
        path::valid_string(path)?;
        let target = self.base.join(path.trim_start_matches('/'));
        path::descendant(&self.base, &target)
            .map_err(|e| format!("Resolve client path {}: {e}", target.display()))
    }
}

impl Drop for Resources {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    #[cfg(any(feature = "plugin-local-forward", feature = "plugin-filesystem"))]
    use tokio::spawn;

    #[test]
    fn client_paths_use_posix_rules_and_require_strict_descendants() {
        use std::fs;

        let dir = tempdir().unwrap();
        let (errors, _messages) = mpsc::unbounded_channel();
        let resources = Resources::new(dir.path().to_owned(), errors);
        let base = fs::canonicalize(dir.path()).unwrap();
        for input in ["preview/file", "/preview/file", "///preview/file"] {
            assert_eq!(
                resources.client_path(input).unwrap(),
                base.join("preview/file")
            );
        }
        for input in ["a\u{feff}b", "a\u{feff}", "/\u{feff}file"] {
            assert_eq!(
                resources.client_path(input).unwrap(),
                base.join(input.trim_start_matches('/'))
            );
        }
        for input in ["", "/", ".", "a/..", "../escape", "a\0b", "\u{feff}b"] {
            assert!(resources.client_path(input).is_err(), "{input:?}");
        }
        for input in [
            "a/../file",
            "https://example.test/file",
            "C:\\file",
            "%2e%2e/file",
        ] {
            assert!(resources.client_path(input).is_ok(), "{input:?}");
        }
    }

    #[tokio::test]
    async fn cancellation_prevents_new_resource_reservations() {
        let dir = tempdir().unwrap();
        let (errors, _messages) = mpsc::unbounded_channel();
        let resources = Resources::new(dir.path().to_owned(), errors);
        resources.cancel();
        assert!(resources.reserve().await.is_err());
    }

    #[cfg(feature = "plugin-local-forward")]
    #[tokio::test]
    async fn clearing_a_pending_binding_wakes_port_waiters_before_task_exit() {
        use tokio::sync::oneshot;
        use tokio::task::yield_now;
        use tokio::time::{Duration, timeout};

        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            let (errors, _messages) = mpsc::unbounded_channel();
            let resources = Arc::new(Resources::new(dir.path().to_owned(), errors));
            let (_binding, outcome) = watch::channel(Binding::Pending);
            let mut resource = Resource::new(Data::Tcp(outcome), &resources.stop, &resources.owner);
            let (finish, finished) = oneshot::channel::<()>();
            resource.task = Some(spawn(async {
                let _ = finished.await;
            }));
            let handle = resources.insert(resource).await.unwrap();
            let waiter = {
                let resources = resources.clone();
                spawn(async move { resources.get_port(handle).await })
            };
            yield_now().await;
            let clear = {
                let resources = resources.clone();
                spawn(async move { resources.clear_forward(handle).await })
            };
            assert_eq!(waiter.await.unwrap().unwrap(), None);
            assert!(!clear.is_finished());
            finish.send(()).unwrap();
            clear.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn shares_handle_namespace_with_overlapping_attachments() {
        use tokio::task::JoinSet;

        use std::collections::BTreeSet;

        let dir = tempdir().unwrap();
        let (errors, _messages) = mpsc::unbounded_channel();
        let handles = Arc::new(AtomicI64::new(0));
        let first = Arc::new(Resources::with_counter(
            dir.path().to_owned(),
            errors.clone(),
            handles.clone(),
        ));
        let second = Arc::new(Resources::with_counter(
            dir.path().to_owned(),
            errors,
            handles,
        ));
        let mut tasks = JoinSet::new();
        for resources in [first, second] {
            for _ in 0..16 {
                let resources = resources.clone();
                tasks.spawn(async move { resources.reserve().await.unwrap() });
            }
        }
        let mut allocated = BTreeSet::new();
        while let Some(handle) = tasks.join_next().await {
            assert!(allocated.insert(handle.unwrap()));
        }
        assert_eq!(allocated.len(), 32);
    }

    #[cfg(feature = "plugin-filesystem")]
    #[tokio::test]
    async fn cleanup_joins_all_workers_before_deleting_any_destination() {
        use tokio::sync::oneshot;
        use tokio::time::{Duration, timeout};

        use std::fs;

        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            let (errors, _messages) = mpsc::unbounded_channel();
            let resources = Arc::new(Resources::new(dir.path().to_owned(), errors));
            let first = dir.path().join("first");
            let second = dir.path().join("second");
            for path in [&first, &second] {
                fs::write(path, b"partial").unwrap();
                let mut resource = Resource::new(
                    Data::Copy {
                        path: path.to_owned(),
                        preserve: Arc::new(AtomicBool::new(false)),
                    },
                    &resources.stop,
                    &resources.owner,
                );
                if path == &second {
                    let (waiting, wait) = oneshot::channel();
                    let (finish, finished) = oneshot::channel::<()>();
                    let stop = resource.stop.clone();
                    resource.task = Some(spawn(async move {
                        stop.cancelled().await;
                        waiting.send(()).unwrap();
                        let _ = finished.await;
                    }));
                    resources.insert(resource).await.unwrap();
                    let cleanup = {
                        let resources = resources.clone();
                        spawn(async move { resources.cleanup().await })
                    };
                    wait.await.unwrap();
                    assert!(first.exists());
                    assert!(second.exists());
                    finish.send(()).unwrap();
                    cleanup.await.unwrap();
                } else {
                    resources.insert(resource).await.unwrap();
                }
            }
            assert!(!first.exists());
            assert!(!second.exists());
            assert!(dir.path().is_dir());
        })
        .await
        .unwrap();
    }

    #[cfg(feature = "plugin-local-forward")]
    #[tokio::test]
    async fn removes_owned_socket_paths_after_a_worker_panics() {
        use tokio::net::UnixListener;

        let dir = tempdir().unwrap();
        let path = dir.path().join("forward.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (errors, mut messages) = mpsc::unbounded_channel();
        let resources = Resources::new(dir.path().to_owned(), errors);
        let mut resource = Resource::new(
            Data::Socket {
                path: path.clone(),
                bound: Arc::new(AtomicBool::new(true)),
            },
            &resources.stop,
            &resources.owner,
        );
        resource.task = Some(spawn(async move {
            drop(listener);
            panic!("Simulated forwarding worker failure");
        }));
        let handle = resources.insert(resource).await.unwrap();
        resources.clear_forward(handle).await;
        assert!(!path.exists());
        assert!(
            messages
                .try_recv()
                .unwrap()
                .contains("Resource task failed")
        );
        assert!(messages.try_recv().is_err());
    }

    #[cfg(feature = "plugin-http")]
    #[tokio::test]
    async fn looks_up_handles_across_attachments_and_cleans_only_owned_resources() {
        let dir = tempdir().unwrap();
        let (errors, _messages) = mpsc::unbounded_channel();
        let handles = Arc::new(AtomicI64::new(0));
        let registry = Arc::new(Registry::default());
        let old = Resources::with_registry(
            dir.path().to_owned(),
            errors.clone(),
            handles.clone(),
            registry.clone(),
        );
        let new = Resources::with_registry(dir.path().to_owned(), errors, handles, registry);
        let earlier = old.reserve().await.unwrap();
        let current = new.reserve().await.unwrap();
        assert_eq!(old.windows().await, [earlier]);
        assert_eq!(new.windows().await, [current]);
        assert!(new.clear_http(earlier).await);
        assert!(old.windows().await.is_empty());
        old.cleanup().await;
        assert_eq!(new.windows().await, [current]);
        assert!(old.clear_http(current).await);
        assert!(new.windows().await.is_empty());
    }
}
