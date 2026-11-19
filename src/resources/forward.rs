use super::path;
use super::{Binding, Data, Kind, Resource, ResourceHandle, Resources};

use tokio::fs::remove_file;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::spawn;
use tokio::sync::watch;
use tokio::task::JoinSet;

use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

impl Resources {
    /// Starts binding a loopback TCP forward and returns its handle.
    pub async fn open_tcp(
        &self,
        client_port: u16,
        host_addr: String,
        host_port: u16,
    ) -> Result<ResourceHandle, String> {
        path::valid_string(&host_addr)?;
        if host_port == 0 {
            return Err("Host TCP port must be between 1 and 65535".to_owned());
        }
        let (binding, outcome) = watch::channel(Binding::Pending);
        let mut resource = Resource::new(Data::Tcp(outcome), &self.stop, &self.owner);
        let stop = resource.stop.clone();
        let errors = self.errors.clone();
        #[cfg(feature = "ssh")]
        let remote = self.remote.clone();
        resource.task = Some(spawn(async move {
            if stop.is_cancelled() {
                return;
            }
            let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, client_port)).await {
                Ok(listener) => listener,
                Err(e) => {
                    let e = format!("Bind local TCP forward on 127.0.0.1:{client_port}: {e}");
                    binding.send_replace(Binding::Failed(e.clone()));
                    let _ = errors.send(e);
                    return;
                }
            };
            match listener.local_addr() {
                Ok(addr) => {
                    binding.send_replace(Binding::Ready(addr.port()));
                }
                Err(e) => {
                    let e = format!("Read local forwarding port: {e}");
                    binding.send_replace(Binding::Failed(e.clone()));
                    let _ = errors.send(e);
                    return;
                }
            }
            let mut clients = JoinSet::new();
            // Drain completed tasks even while the accept queue stays ready.
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = clients.join_next(), if !clients.is_empty() => {},
                    accepted = listener.accept() => {
                        let (mut client, origin) = match accepted {
                            Ok(accepted) => accepted,
                            Err(e) => {
                                let _ = errors.send(format!("Accept local TCP forward: {e}"));
                                break;
                            }
                        };
                        let host_addr = host_addr.clone();
                        let errors = errors.clone();
                        let stop = stop.clone();
                        #[cfg(feature = "ssh")]
                        let remote = remote.clone();
                        clients.spawn(async move {
                            let relay = async move {
                                #[cfg(feature = "ssh")]
                                if let Some(remote) = remote {
                                    let mut host = remote.handle()
                                        .channel_open_direct_tcpip(host_addr, u32::from(host_port), origin.ip().to_string(), u32::from(origin.port()))
                                        .await.map_err(|e| format!("Open SSH TCP forwarding channel: {e}"))?
                                        .into_stream();
                                    copy_bidirectional(&mut client, &mut host).await
                                        .map_err(|e| format!("Relay SSH TCP forward: {e}"))?;
                                    return Ok::<(), String>(());
                                }
                                let _ = origin;
                                let mut host = TcpStream::connect((host_addr.as_str(), host_port)).await
                                    .map_err(|e| format!("Connect TCP forward to {host_addr}:{host_port}: {e}"))?;
                                copy_bidirectional(&mut client, &mut host).await
                                    .map_err(|e| format!("Relay TCP forward: {e}"))?;
                                Ok(())
                            };
                            tokio::select! {
                                biased;
                                _ = stop.cancelled() => {},
                                ret = relay => if let Err(e) = ret { let _ = errors.send(e); },
                            }
                        });
                    }
                }
            }
            drop(listener);
            clients.abort_all();
            while clients.join_next().await.is_some() {}
        }));
        self.insert(resource).await
    }

    /// Starts binding a Unix-domain listener and returns its handle.
    pub async fn open_socket(
        &self,
        client_socket_path: String,
        host_socket_path: String,
    ) -> Result<ResourceHandle, String> {
        path::valid_string(&host_socket_path)?;
        let path = self.client_path(&client_socket_path)?;
        let bound = Arc::new(AtomicBool::new(false));
        let mut resource = Resource::new(
            Data::Socket {
                path: path.clone(),
                bound: bound.clone(),
            },
            &self.stop,
            &self.owner,
        );
        let base = self.base.clone();
        let stop = resource.stop.clone();
        let errors = self.errors.clone();
        #[cfg(feature = "ssh")]
        let remote = self.remote.clone();
        resource.task = Some(spawn(async move {
            if stop.is_cancelled() {
                return;
            }
            let listener = match UnixListener::bind(&path) {
                Ok(listener) => listener,
                Err(e) => {
                    let _ =
                        errors.send(format!("Bind local socket forward {}: {e}", path.display()));
                    return;
                }
            };
            bound.store(true, Ordering::Release);
            let mut clients = JoinSet::new();
            // Drain completed tasks even while the accept queue stays ready.
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = clients.join_next(), if !clients.is_empty() => {},
                    accepted = listener.accept() => {
                        let (mut client, _) = match accepted {
                            Ok(accepted) => accepted,
                            Err(e) => {
                                let _ = errors.send(format!("Accept local socket forward: {e}"));
                                break;
                            }
                        };
                        let path = host_socket_path.clone();
                        let errors = errors.clone();
                        let stop = stop.clone();
                        #[cfg(feature = "ssh")]
                        let remote = remote.clone();
                        clients.spawn(async move {
                            let relay = async move {
                                #[cfg(feature = "ssh")]
                                if let Some(remote) = remote {
                                    let mut host = remote.handle().channel_open_direct_streamlocal(path).await
                                        .map_err(|e| format!("Open SSH socket forwarding channel: {e}"))?.into_stream();
                                    copy_bidirectional(&mut client, &mut host).await
                                        .map_err(|e| format!("Relay SSH socket forward: {e}"))?;
                                    return Ok::<(), String>(());
                                }
                                let mut host = UnixStream::connect(&path).await
                                    .map_err(|e| format!("Connect socket forward to {path}: {e}"))?;
                                copy_bidirectional(&mut client, &mut host).await
                                    .map_err(|e| format!("Relay socket forward: {e}"))?;
                                Ok(())
                            };
                            tokio::select! {
                                biased;
                                _ = stop.cancelled() => {},
                                ret = relay => if let Err(e) = ret { let _ = errors.send(e); },
                            }
                        });
                    }
                }
            }
            drop(listener);
            clients.abort_all();
            while clients.join_next().await.is_some() {}
            if bound.swap(false, Ordering::AcqRel)
                && path::descendant(&base, &path).is_ok()
                && let Err(e) = remove_file(&path).await
                && e.kind() != ErrorKind::NotFound
            {
                let _ = errors.send(format!(
                    "Remove local forwarding socket {}: {e}",
                    path.display()
                ));
            }
        }));
        self.insert(resource).await
    }

    /// Waits asynchronously for a TCP forward's binding outcome.
    ///
    /// Returns `None` if the pending operation is cleared while waiting.
    pub async fn get_port(&self, handle: ResourceHandle) -> Result<Option<u16>, String> {
        self.port(handle, Kind::Tcp).await
    }

    /// Cancels a pending forward or closes its listener and active relays.
    pub async fn clear_forward(&self, handle: ResourceHandle) {
        if let Some(resource) = self.take(handle, &[Kind::Tcp, Kind::Socket]).await {
            self.stop(resource).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::mpsc;
    use tokio::task::yield_now;
    use tokio::time::timeout;

    use tempfile::tempdir;

    use std::fs;
    use std::time::Duration;

    #[tokio::test]
    async fn forwards_both_directions_and_closes_listener() {
        timeout(Duration::from_secs(5), async {
            let host = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let host_port = host.local_addr().unwrap().port();
            let host_task = spawn(async move {
                let (mut socket, _) = host.accept().await.unwrap();
                let mut input = Vec::new();
                socket.read_to_end(&mut input).await.unwrap();
                assert_eq!(input, b"request");
                socket.write_all(b"response").await.unwrap();
            });
            let dir = tempdir().unwrap();
            let (errors, mut messages) = mpsc::unbounded_channel();
            let resources = Resources::new(dir.path().to_owned(), errors);
            let handle = resources
                .open_tcp(0, "127.0.0.1".to_owned(), host_port)
                .await
                .unwrap();
            let port = resources.get_port(handle).await.unwrap().unwrap();
            let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            socket.write_all(b"request").await.unwrap();
            socket.shutdown().await.unwrap();
            let mut output = Vec::new();
            socket.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, b"response");
            host_task.await.unwrap();
            resources.clear_forward(handle).await;
            // Concurrent subprocess tests can briefly inherit a CLOEXEC socket
            // between fork and exec. Verify that the listener becomes bindable.
            loop {
                match TcpListener::bind(("127.0.0.1", port)).await {
                    Ok(listener) => {
                        drop(listener);
                        break;
                    }
                    Err(e) if e.kind() == ErrorKind::AddrInUse => {
                        yield_now().await;
                    }
                    Err(e) => panic!("Rebind cleared forwarding port: {e}"),
                }
            }
            assert!(resources.get_port(handle).await.is_err());
            assert!(messages.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn retains_binding_failure_for_get_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempdir().unwrap();
        let (errors, mut messages) = mpsc::unbounded_channel();
        let resources = Resources::new(dir.path().to_owned(), errors);
        let handle = resources
            .open_tcp(port, "127.0.0.1".to_owned(), 80)
            .await
            .unwrap();
        let error = resources.get_port(handle).await.unwrap_err();
        assert!(error.contains("Bind local TCP forward"));
        assert_eq!(resources.get_port(handle).await.unwrap_err(), error);
        assert_eq!(messages.recv().await.unwrap(), error);
        let reserved = resources.reserve().await.unwrap();
        assert_ne!(reserved, handle);
        assert!(resources.get_port(reserved).await.is_err());
        resources.cleanup().await;
    }

    #[tokio::test]
    async fn forwards_unix_sockets_under_the_temporary_directory_and_cleans_up() {
        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            let host_path = dir.path().join("host.sock");
            let host = UnixListener::bind(&host_path).unwrap();
            let base = dir.path().join("client");
            fs::create_dir(&base).unwrap();
            let (errors, mut messages) = mpsc::unbounded_channel();
            let resources = Resources::new(base.clone(), errors);
            for input in ["", ".", "../outside"] {
                assert!(
                    resources
                        .open_socket(input.to_owned(), host_path.to_str().unwrap().to_owned())
                        .await
                        .is_err()
                );
            }
            let handle = resources
                .open_socket(
                    "/forward.sock".to_owned(),
                    host_path.to_str().unwrap().to_owned(),
                )
                .await
                .unwrap();
            let path = base.join("forward.sock");
            while !path.exists() {
                yield_now().await;
            }
            let mut client = UnixStream::connect(&path).await.unwrap();
            let host_task = spawn(async move {
                let (mut host, _) = host.accept().await.unwrap();
                let mut input = [0; 4];
                host.read_exact(&mut input).await.unwrap();
                assert_eq!(&input, b"ping");
                host.write_all(b"pong").await.unwrap();
            });
            client.write_all(b"ping").await.unwrap();
            let mut output = [0; 4];
            client.read_exact(&mut output).await.unwrap();
            assert_eq!(&output, b"pong");
            host_task.await.unwrap();
            resources.clear_forward(handle).await;
            assert!(!path.exists());
            resources.clear_forward(handle).await;
            assert!(messages.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_socket_binding_preserves_the_preexisting_path() {
        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            let path = dir.path().join("forward.sock");
            let _existing = UnixListener::bind(&path).unwrap();
            let (errors, mut messages) = mpsc::unbounded_channel();
            let resources = Resources::new(dir.path().to_owned(), errors);
            let handle = resources
                .open_socket("forward.sock".to_owned(), "/unused".to_owned())
                .await
                .unwrap();
            assert!(
                messages
                    .recv()
                    .await
                    .unwrap()
                    .contains("Bind local socket forward")
            );
            resources.clear_forward(handle).await;
            assert!(path.exists());
            assert!(messages.try_recv().is_err());
        })
        .await
        .unwrap();
    }
}
