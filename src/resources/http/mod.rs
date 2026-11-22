mod wire;
use wire::OriginIo;
mod webview;
use webview::WebviewNavigation;
pub use webview::WebviewWindow;
pub(super) use webview::{ServedOrigins, Window};

use crate::config::WebviewPolicy;

use super::{Binding, Data, Kind, Resource, ResourceHandle, Resources};

use bytes::{Bytes, BytesMut};
use hyper::body::{self, Frame, SizeHint};
use hyper::header::{
    ACCESS_CONTROL_ALLOW_ORIGIN, ALLOW, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue,
};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::fs::{self, File};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::spawn;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

enum ServingRoot {
    File(PathBuf),
    Directory(PathBuf),
}

impl ServingRoot {
    fn path(&self) -> &Path {
        match self {
            Self::File(path) | Self::Directory(path) => path,
        }
    }

    fn contains(&self, target: &Path) -> bool {
        match self {
            Self::File(path) => target == path,
            Self::Directory(path) => target.starts_with(path),
        }
    }
}

struct FileBody {
    file: Option<File>,
    remaining: u64,
    buf: BytesMut,
}

impl FileBody {
    fn empty() -> Self {
        Self {
            file: None,
            remaining: 0,
            buf: BytesMut::new(),
        }
    }
}

impl body::Body for FileBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let Self {
            file,
            remaining,
            buf,
        } = self.get_mut();
        if *remaining == 0 {
            return Poll::Ready(None);
        }
        let Some(file) = file else {
            return Poll::Ready(None);
        };
        let len = usize::try_from((*remaining).min(65536)).unwrap_or(65536);
        buf.resize(len, 0);
        let mut read = ReadBuf::new(buf);
        match Pin::new(file).poll_read(cx, &mut read) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => {
                *remaining = 0;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(Ok(())) => {
                let n = read.filled().len();
                if n == 0 {
                    *remaining = 0;
                    return Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Served file shortened during transfer",
                    ))));
                }
                *remaining = remaining.saturating_sub(n as u64);
                // ReadBuf reports at most the supplied buffer length, so `n`
                // is at most buf.len() and split_to cannot panic.
                Poll::Ready(Some(Ok(Frame::data(buf.split_to(n).freeze()))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

impl Resources {
    /// Validates the serving root and starts a loopback HTTP(S) server.
    ///
    /// HTTPS uses a generated self-signed certificate. `http2` selects HTTP/2
    /// exclusively and requires HTTPS. HTTP/1.1 supports plain HTTP and HTTPS.
    pub async fn serve_file(
        &self,
        client_file: String,
        client_port: u16,
        https: bool,
        http2: bool,
    ) -> Result<ResourceHandle, String> {
        let root = self.client_path(&client_file)?;
        let root = fs::canonicalize(&root)
            .await
            .map_err(|e| format!("Resolve HTTP serving root {}: {e}", root.display()))?;
        let metadata = fs::metadata(&root)
            .await
            .map_err(|e| format!("Inspect HTTP serving root {}: {e}", root.display()))?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Err("HTTP serving root must be a regular file or directory".to_owned());
        }
        if http2 && !https {
            return Err("HTTP/2 requires HTTPS".to_owned());
        }
        let root = if metadata.is_file() {
            ServingRoot::File(root)
        } else {
            ServingRoot::Directory(root)
        };
        let (binding, outcome) = watch::channel(Binding::Pending);
        let mut resource = Resource::new(
            Data::Http {
                path: root.path().to_owned(),
                binding: outcome,
            },
            &self.stop,
            &self.owner,
        );
        let stop = resource.stop.clone();
        let errors = self.errors.clone();
        let origins = Arc::clone(&self.served_origins);
        resource.task = Some(spawn(async move {
            if stop.is_cancelled() {
                return;
            }
            let startup = async {
                let tls = if https {
                    Some(tls_acceptor(http2)?)
                } else {
                    None
                };
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, client_port))
                    .await
                    .map_err(|e| format!("Bind HTTP server on 127.0.0.1:{client_port}: {e}"))?;
                let port = listener
                    .local_addr()
                    .map_err(|e| format!("Read HTTP server port: {e}"))?
                    .port();
                Ok::<_, String>((listener, port, tls))
            };
            let (listener, port, tls) = tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                ret = startup => match ret {
                    Ok(ret) => ret,
                    Err(e) => { binding.send_replace(Binding::Failed(e.clone())); let _ = errors.send(e); return; }
                }
            };
            let active_origin = origins.register(https, port, stop.clone());
            binding.send_replace(Binding::Ready(port));
            let scheme = if https { "https" } else { "http" };
            let origin = format!("{scheme}://127.0.0.1:{port}");
            let root = Arc::new(root);
            let mut clients = JoinSet::new();
            // Drain completed tasks even while the accept queue stays ready.
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = clients.join_next(), if !clients.is_empty() => {},
                    accepted = listener.accept() => match accepted {
                        Ok((socket, _)) => {
                            let root = root.clone();
                            let origin = origin.clone();
                            let tls = tls.clone();
                            let stop = stop.clone();
                            let errors = errors.clone();
                            clients.spawn(async move {
                                let serving = serve_connection(socket, tls, http2, root, origin);
                                tokio::select! {
                                    biased;
                                    _ = stop.cancelled() => {},
                                    ret = serving => if let Err(e) = ret { let _ = errors.send(e); },
                                }
                            });
                        }
                        Err(e) => { let _ = errors.send(format!("Accept HTTP connection: {e}")); break; }
                    },
                }
            }
            drop(active_origin);
            drop(listener);
            clients.abort_all();
            while clients.join_next().await.is_some() {}
        }));
        self.insert(resource).await
    }

    /// Waits asynchronously for an HTTP server's assigned listener port.
    ///
    /// Returns `None` if the pending operation is cleared while waiting.
    pub async fn get_http_port(&self, handle: ResourceHandle) -> Result<Option<u16>, String> {
        self.port(handle, Kind::Http).await
    }

    pub(crate) fn webview_navigation(&self, policy: WebviewPolicy) -> WebviewNavigation {
        WebviewNavigation::new(policy, Arc::clone(&self.served_origins))
    }

    pub(crate) async fn reserve_webview(
        &self,
        navigation: WebviewNavigation,
        focusable: bool,
    ) -> Result<(ResourceHandle, WebviewWindow), String> {
        let window = WebviewWindow::new(
            navigation,
            self.stop.clone(),
            self.errors.clone(),
            focusable,
        );
        let resource = Resource::new(
            Data::Webview(Arc::downgrade(&window.0)),
            &self.stop,
            &self.owner,
        );
        let handle = self.insert(resource).await?;
        Ok((handle, window))
    }

    pub(crate) async fn can_focus_webview(&self, handle: ResourceHandle) -> bool {
        let state = self.registry.state.lock().await;
        let Some(Resource {
            data: Data::Webview(window),
            ..
        }) = state.entries.get(&handle)
        else {
            return false;
        };

        window.upgrade().is_some_and(|window| window.can_focus())
    }

    /// Cancels server startup, closes the HTTP listener or clears an open window.
    ///
    /// Returns whether a window association was removed. Close that window
    /// through the frontend when this returns `true`.
    pub async fn clear_http(&self, handle: ResourceHandle) -> bool {
        let kinds = [Kind::Http, Kind::Reserved, Kind::Webview];
        let Some(resource) = self
            .take_if(handle, &kinds, |resource| match &resource.data {
                Data::Webview(window) => window.upgrade().is_some_and(|window| window.close_open()),
                _ => true,
            })
            .await
        else {
            return false;
        };

        let window = matches!(resource.data, Data::Reserved | Data::Webview(_));
        self.stop(resource).await;
        window
    }

    /// Returns handles for windows associated with this attachment.
    pub async fn windows(&self) -> Vec<ResourceHandle> {
        self.registry
            .state
            .lock()
            .await
            .entries
            .iter()
            .filter_map(|(handle, resource)| {
                if !Arc::ptr_eq(&resource.owner, &self.owner) {
                    return None;
                }
                let associated = match &resource.data {
                    Data::Reserved => true,
                    Data::Webview(window) => window
                        .upgrade()
                        .is_some_and(|window| window.is_associated()),
                    _ => false,
                };
                associated.then_some(*handle)
            })
            .collect()
    }
}

fn tls_acceptor(http2: bool) -> Result<TlsAcceptor, String> {
    use rcgen::generate_simple_self_signed;

    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::crypto::ring;
    use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;

    let cert = generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
        .map_err(|e| format!("Generate HTTP certificate: {e}"))?;
    let key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let provider = ring::default_provider();
    let mut config = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key.into())
        .map_err(|e| e.to_string())?;
    config.alpn_protocols = vec![if http2 {
        b"h2".to_vec()
    } else {
        b"http/1.1".to_vec()
    }];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn serve_connection(
    socket: TcpStream,
    tls: Option<TlsAcceptor>,
    http2: bool,
    root: Arc<ServingRoot>,
    origin: String,
) -> Result<(), String> {
    if let Some(tls) = tls {
        let stream = tls
            .accept(socket)
            .await
            .map_err(|e| format!("Accept HTTPS handshake: {e}"))?;
        serve_stream(stream, http2, root, origin).await
    } else {
        serve_stream(socket, http2, root, origin).await
    }
}

async fn serve_stream<S>(
    stream: S,
    http2: bool,
    root: Arc<ServingRoot>,
    origin: String,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request: Request<_>| {
        let root = root.clone();
        let origin = origin.clone();
        async move {
            if !http2 && request.version() != Version::HTTP_11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Only HTTP/1.1 is supported",
                ));
            }
            Ok(response(request, &root, &origin, http2).await)
        }
    });
    if http2 {
        http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), service)
            .await
            .map_err(|e| format!("Serve HTTP/2 connection: {e}"))
    } else {
        http1::Builder::new()
            .serve_connection(TokioIo::new(OriginIo::new(stream)), service)
            .await
            .map_err(|e| format!("Serve HTTP/1.1 connection: {e}"))
    }
}

async fn response<B>(
    request: Request<B>,
    root: &ServingRoot,
    origin: &str,
    http2: bool,
) -> Response<FileBody> {
    let mut ret = file_response(&request, root, http2).await;
    if let Ok(origin) = origin.parse() {
        ret.headers_mut()
            .insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    ret
}

fn empty(status: StatusCode) -> Response<FileBody> {
    let mut ret = Response::new(FileBody::empty());
    *ret.status_mut() = status;
    ret
}

async fn file_response<B>(
    request: &Request<B>,
    root: &ServingRoot,
    http2: bool,
) -> Response<FileBody> {
    let head = request.method() == Method::HEAD && http2;
    if request.method() != Method::GET && !head {
        let mut ret = empty(StatusCode::METHOD_NOT_ALLOWED);
        ret.headers_mut().insert(
            ALLOW,
            HeaderValue::from_static(if http2 { "GET, HEAD" } else { "GET" }),
        );
        return ret;
    }
    if request.headers().get_all(CONNECTION).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        })
    }) {
        return empty(StatusCode::BAD_REQUEST);
    }
    let Some(path) = decode_path(request.uri().path()) else {
        return empty(StatusCode::BAD_REQUEST);
    };
    let Some(path) = path.strip_prefix('/') else {
        return empty(StatusCode::BAD_REQUEST);
    };
    let target = if path.is_empty() {
        root.path().to_owned()
    } else {
        root.path().join(path.trim_start_matches('/'))
    };
    let target = match resolve_missing(&target).await {
        Ok(target) => target,
        Err(e) => {
            return empty(
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
            );
        }
    };
    if !root.contains(&target) {
        return empty(StatusCode::BAD_REQUEST);
    }
    match fs::metadata(&target).await {
        Ok(metadata) if !metadata.is_file() => return empty(StatusCode::BAD_REQUEST),
        Ok(_) => {}
        Err(e) => {
            return empty(
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
            );
        }
    }
    let file = match File::open(&target).await {
        Ok(file) => file,
        Err(e) => {
            return empty(
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
            );
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(_) => return empty(StatusCode::INTERNAL_SERVER_ERROR),
    };
    if !metadata.is_file() {
        return empty(StatusCode::BAD_REQUEST);
    }
    // Path resolution alone is insufficient: a symlink can be replaced before
    // open. Inspect the actual opened descriptor before exposing any bytes.
    match opened_path(&file) {
        Ok(path) if root.contains(&path) => {}
        Ok(_) => return empty(StatusCode::BAD_REQUEST),
        Err(_) => return empty(StatusCode::INTERNAL_SERVER_ERROR),
    }
    let len = metadata.len();
    let body = if head {
        FileBody::empty()
    } else {
        FileBody {
            file: Some(file),
            remaining: len,
            buf: BytesMut::new(),
        }
    };
    let mut ret = Response::new(body);
    ret.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(content_type(&target)),
    );
    if let Ok(len) = len.to_string().parse() {
        ret.headers_mut().insert(CONTENT_LENGTH, len);
    }
    ret
}

#[cfg(target_os = "linux")]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::fs::read_link;
    use std::os::fd::AsRawFd as _;

    read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(target_os = "macos")]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use libc::{F_GETPATH, PATH_MAX, fcntl};

    use std::ffi::{CStr, OsStr};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let mut buf = [0_u8; PATH_MAX as usize];
    // F_GETPATH writes at most PATH_MAX bytes into the supplied buffer.
    let ret = unsafe { fcntl(file.as_raw_fd(), F_GETPATH, buf.as_mut_ptr()) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let path = CStr::from_bytes_until_nul(&buf).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Opened file path is not terminated",
        )
    })?;
    Ok(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn opened_path(_: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Opened-file confinement is unavailable on this platform",
    ))
}

fn decode_path(path: &str) -> Option<String> {
    let mut ret = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            let byte = u8::try_from(high.checked_mul(16)?.checked_add(low)?).ok()?;
            if byte == 0 {
                return None;
            }
            ret.push(byte);
        } else {
            ret.push(byte);
        }
    }
    String::from_utf8(ret).ok()
}

async fn resolve_missing(path: &Path) -> io::Result<PathBuf> {
    use super::path::resolve;

    use tokio::task::spawn_blocking;

    let path = path.to_owned();
    spawn_blocking(move || resolve(&path))
        .await
        .map_err(io::Error::other)?
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|name| name.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hyper::body::Body as _;

    use http_body_util::{BodyExt as _, Empty};
    use tempfile::tempdir;

    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use std::fs as std_fs;
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    #[test]
    fn decodes_path_once_without_reparsing_delimiters() {
        assert_eq!(
            decode_path("/a%23b%3Fc%20%E6%97%A5"),
            Some("/a#b?c 日".to_owned())
        );
        assert_eq!(decode_path("/%252e%252e"), Some("/%2e%2e".to_owned()));
        assert_eq!(decode_path("/%00"), None);
        assert_eq!(decode_path("/%z0"), None);
    }

    #[tokio::test]
    async fn serves_files_and_confines_symlinks() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).await.unwrap();
        fs::write(root.join("a#b?c 日"), b"hello").await.unwrap();
        fs::write(dir.path().join("outside"), b"secret")
            .await
            .unwrap();
        symlink(dir.path().join("outside"), root.join("link")).unwrap();
        let serving = ServingRoot::Directory(root.clone());
        for (path, status) in [
            ("/a%23b%3Fc%20%E6%97%A5", StatusCode::OK),
            ("/", StatusCode::BAD_REQUEST),
            ("/missing", StatusCode::BAD_REQUEST),
            ("/link", StatusCode::BAD_REQUEST),
            ("/../outside", StatusCode::BAD_REQUEST),
        ] {
            let request = Request::builder().uri(path).body(()).unwrap();
            let ret = response(request, &serving, "http://127.0.0.1:1234", true).await;
            assert_eq!(ret.status(), status, "{path}");
            assert_eq!(
                ret.headers()[ACCESS_CONTROL_ALLOW_ORIGIN],
                "http://127.0.0.1:1234"
            );
            if status != StatusCode::OK {
                assert_eq!(ret.body().size_hint().exact(), Some(0));
            }
        }
        let ret = response(
            Request::builder()
                .method(Method::HEAD)
                .uri("/a%23b%3Fc%20%E6%97%A5")
                .body(())
                .unwrap(),
            &serving,
            "http://127.0.0.1:1234",
            true,
        )
        .await;
        assert_eq!(ret.headers()[CONTENT_LENGTH], "5");
        assert!(ret.body().is_end_stream());
    }

    #[tokio::test]
    async fn never_serves_outside_bytes_when_a_path_is_replaced_by_a_symlink() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        std_fs::create_dir(&root).unwrap();
        std_fs::write(dir.path().join("outside"), b"secret").unwrap();
        std_fs::write(root.join("changing"), b"public").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_root = root.clone();
        let outside = dir.path().join("outside");
        let serving = ServingRoot::Directory(root.clone());
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                let next = worker_root.join("next");
                if symlink(&outside, &next).is_err() {
                    break;
                }
                if std_fs::rename(&next, worker_root.join("changing")).is_err() {
                    break;
                }
                if std_fs::write(&next, b"public").is_err() {
                    break;
                }
                if std_fs::rename(&next, worker_root.join("changing")).is_err() {
                    break;
                }
            }
        });
        let mut leaked = false;
        for _ in 0..200 {
            let response = file_response(
                &Request::builder().uri("/changing").body(()).unwrap(),
                &serving,
                true,
            )
            .await;
            if let Ok(body) = response.into_body().collect().await {
                leaked |= body.to_bytes() == "secret";
            }
        }
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        assert!(!leaked);
    }

    #[tokio::test]
    async fn file_roots_never_expand_their_scope_to_descendants() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("root");
        fs::write(&path, b"original").await.unwrap();
        let root = ServingRoot::File(path.clone());
        let response =
            file_response(&Request::builder().uri("/").body(()).unwrap(), &root, true).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "original"
        );
        fs::remove_file(&path).await.unwrap();
        fs::create_dir(&path).await.unwrap();
        fs::write(path.join("child"), b"outside the file scope")
            .await
            .unwrap();
        let response = file_response(
            &Request::builder().uri("/child").body(()).unwrap(),
            &root,
            true,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .len(),
            0
        );
    }

    #[derive(Debug)]
    struct LoopbackCertificate(tokio_rustls::rustls::crypto::WebPkiSupportedAlgorithms);

    impl tokio_rustls::rustls::client::danger::ServerCertVerifier for LoopbackCertificate {
        fn verify_server_cert(
            &self,
            _: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
            _: &tokio_rustls::rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: tokio_rustls::rustls::pki_types::UnixTime,
        ) -> Result<
            tokio_rustls::rustls::client::danger::ServerCertVerified,
            tokio_rustls::rustls::Error,
        > {
            // The server creates a new self-signed certificate at startup.
            Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            signed: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            tokio_rustls::rustls::crypto::verify_tls12_signature(message, cert, signed, &self.0)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            signed: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            tokio_rustls::rustls::crypto::verify_tls13_signature(message, cert, signed, &self.0)
        }

        fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
            self.0.supported_schemes()
        }
    }

    async fn connect_https(
        socket: TcpStream,
        alpn: &[u8],
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        use tokio_rustls::{
            TlsConnector,
            rustls::{ClientConfig, crypto::ring, pki_types::ServerName},
        };

        let provider = ring::default_provider();
        let verifier = LoopbackCertificate(provider.signature_verification_algorithms);
        let mut config = ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![alpn.to_vec()];
        let stream = TlsConnector::from(Arc::new(config))
            .connect(ServerName::from(Ipv4Addr::LOCALHOST), socket)
            .await
            .unwrap();
        assert_eq!(stream.get_ref().1.alpn_protocol(), Some(alpn));
        stream
    }

    #[tokio::test]
    async fn serves_the_three_supported_protocol_combinations_on_loopback() {
        use hyper::client::conn::{http1 as client_http1, http2 as client_http2};

        async fn get_http1<S>(socket: S, request: Request<Empty<Bytes>>) -> Response<body::Incoming>
        where
            S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        {
            let (mut client, connection) =
                client_http1::handshake(TokioIo::new(socket)).await.unwrap();
            spawn(connection);
            client.send_request(request).await.unwrap()
        }

        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            fs::write(dir.path().join("hello.html"), b"<h1>Hello</h1>")
                .await
                .unwrap();
            let (errors, mut messages) = mpsc::unbounded_channel();
            let resources = Resources::new(dir.path().to_owned(), errors);
            let error = resources
                .serve_file("hello.html".to_owned(), 0, false, true)
                .await
                .unwrap_err();
            assert_eq!(error, "HTTP/2 requires HTTPS");
            assert!(resources.registry.state.lock().await.entries.is_empty());

            for (https, http2) in [(false, false), (true, false), (true, true)] {
                let handle = resources
                    .serve_file("/hello.html".to_owned(), 0, https, http2)
                    .await
                    .unwrap();
                let port = resources.get_http_port(handle).await.unwrap().unwrap();
                let socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .await
                    .unwrap();
                let scheme = if https { "https" } else { "http" };
                let origin = format!("{scheme}://127.0.0.1:{port}");
                let request = Request::builder()
                    .uri(format!("{origin}/"))
                    .header("host", format!("127.0.0.1:{port}"))
                    .version(if http2 {
                        Version::HTTP_2
                    } else {
                        Version::HTTP_11
                    })
                    .body(Empty::<Bytes>::new())
                    .unwrap();
                let expected_version = request.version();
                let response = if http2 {
                    let tls = connect_https(socket, b"h2").await;
                    let (mut client, connection) =
                        client_http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
                            .await
                            .unwrap();
                    spawn(connection);
                    client.send_request(request).await.unwrap()
                } else if https {
                    get_http1(connect_https(socket, b"http/1.1").await, request).await
                } else {
                    get_http1(socket, request).await
                };
                assert_eq!(response.version(), expected_version);
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
                assert_eq!(response.headers()[CONTENT_LENGTH], "14");
                assert_eq!(response.headers()[ACCESS_CONTROL_ALLOW_ORIGIN], origin);
                assert_eq!(
                    response.into_body().collect().await.unwrap().to_bytes(),
                    "<h1>Hello</h1>"
                );
                resources.clear_http(handle).await;
                assert!(resources.get_http_port(handle).await.is_err());
            }
            resources.cleanup().await;
            assert!(messages.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn navigation_consults_active_file_servers_from_its_own_attachment() {
        use crate::Policies;

        use mlua::Lua;

        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file"), b"test").await.unwrap();
        let (errors, mut messages) = mpsc::unbounded_channel();
        let first = Resources::new(dir.path().to_owned(), errors.clone());
        let other = Resources::with_registry(
            dir.path().to_owned(),
            errors,
            Arc::clone(&first.handles),
            Arc::clone(&first.registry),
        );
        let lua = Lua::new();
        let config = lua
            .load(
                r#"return {plugin = {http = {open_webview = {
                origins = {"https://docs.example.test"}, served_files = true,
            }}}}"#,
            )
            .eval()
            .unwrap();
        let policy = Policies::parse(Some(config)).unwrap().webview;
        let (_, window) = first
            .reserve_webview(first.webview_navigation(policy.clone()), true)
            .await
            .unwrap();
        let (_, other_window) = other
            .reserve_webview(other.webview_navigation(policy), true)
            .await
            .unwrap();

        assert!(window.allows_navigation("https://docs.example.test:443/start"));
        assert!(window.opened());
        for (https, scheme, wrong_scheme) in [(false, "http", "https"), (true, "https", "http")] {
            let server = first
                .serve_file("file".to_owned(), 0, https, false)
                .await
                .unwrap();
            let port = timeout(Duration::from_secs(5), first.get_http_port(server))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let url = format!("{scheme}://127.0.0.1:{port}/redirect#fragment");
            assert!(window.allows_navigation(&url));
            for (window, url) in [
                (&other_window, url.clone()),
                (&window, format!("{wrong_scheme}://127.0.0.1:{port}/")),
                (&window, format!("{scheme}://localhost:{port}/")),
            ] {
                assert!(!window.allows_navigation(&url));
                assert_eq!(
                    messages.try_recv().unwrap(),
                    format!("Client policy denies Webview navigation to {url:?}")
                );
            }

            first.clear_http(server).await;
            assert!(!window.allows_navigation(&url));
            assert_eq!(
                messages.try_recv().unwrap(),
                format!("Client policy denies Webview navigation to {url:?}")
            );
        }
        first.cleanup().await;
        other.cleanup().await;
        assert!(!window.allows_navigation("https://docs.example.test/after-cleanup"));
        assert!(messages.try_recv().is_err());
    }

    #[tokio::test]
    async fn http1_server_does_not_serve_http_10() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, duplex};

        timeout(Duration::from_secs(5), async {
            let (mut client, server) = duplex(8192);
            let server = spawn(serve_stream(
                server,
                false,
                Arc::new(ServingRoot::Directory(PathBuf::from("/unused"))),
                "http://127.0.0.1:1234".to_owned(),
            ));
            client.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert!(response.is_empty());
            assert!(server.await.unwrap().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reports_binding_failure_and_releases_the_port_on_clear() {
        use tokio::task::yield_now;

        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            fs::write(dir.path().join("hello.html"), b"<h1>Hello</h1>")
                .await
                .unwrap();
            let (errors, mut messages) = mpsc::unbounded_channel();
            let resources = Resources::new(dir.path().to_owned(), errors);
            let handle = resources
                .serve_file("hello.html".to_owned(), 0, false, false)
                .await
                .unwrap();
            let port = resources.get_http_port(handle).await.unwrap().unwrap();
            let busy = resources
                .serve_file("hello.html".to_owned(), port, false, false)
                .await
                .unwrap();
            let error = resources.get_http_port(busy).await.unwrap_err();
            assert!(error.contains("Bind HTTP server"));
            assert_eq!(messages.recv().await.unwrap(), error);
            resources.clear_http(handle).await;
            loop {
                match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                    Ok(_) => break,
                    Err(e) if e.kind() == io::ErrorKind::AddrInUse => yield_now().await,
                    Err(e) => panic!("Rebind cleared HTTP port: {e}"),
                }
            }
            resources.cleanup().await;
            assert!(resources.reserve().await.is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn clearing_pending_http_startup_completes_a_port_query() {
        use std::future::{Future, poll_fn};

        let dir = tempdir().unwrap();
        let (errors, _messages) = mpsc::unbounded_channel();
        let resources = Resources::new(dir.path().to_owned(), errors);
        let (_binding, outcome) = watch::channel(Binding::Pending);
        let resource = Resource::new(
            Data::Http {
                path: fs::canonicalize(dir.path()).await.unwrap().join("file"),
                binding: outcome,
            },
            &resources.stop,
            &resources.owner,
        );
        let handle = resources.insert(resource).await.unwrap();
        let mut getting_port = std::pin::pin!(resources.get_http_port(handle));
        poll_fn(|cx| {
            assert!(getting_port.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        resources.clear_http(handle).await;
        assert_eq!(
            timeout(Duration::from_secs(5), getting_port)
                .await
                .unwrap()
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn serves_percent_encoded_ascii_path_data_without_reinterpreting_delimiters() {
        use std::fmt::Write as _;
        let dir = tempdir().unwrap();
        let root = ServingRoot::Directory(std_fs::canonicalize(dir.path()).unwrap());
        for byte in 0x20_u8..=0x7e {
            let name = if byte == b'/' {
                "a/b".to_owned()
            } else {
                format!("a{}b", char::from(byte))
            };
            let target = dir.path().join(&name);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).await.unwrap();
            }
            fs::write(&target, [byte]).await.unwrap();
            let mut uri = String::from("/a");
            write!(uri, "%{byte:02X}b").unwrap();
            let ret =
                file_response(&Request::builder().uri(uri).body(()).unwrap(), &root, true).await;
            assert_eq!(ret.status(), StatusCode::OK, "{name:?}");
            assert_eq!(
                ret.into_body().collect().await.unwrap().to_bytes().as_ref(),
                [byte],
                "{name:?}"
            );
            fs::remove_file(target).await.unwrap();
        }
    }

    #[tokio::test]
    async fn serving_roots_and_clear_targets_remain_confined() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("client");
        let outside = dir.path().join("outside");
        fs::create_dir(&base).await.unwrap();
        fs::write(&outside, b"secret").await.unwrap();
        symlink(&outside, base.join("escape")).unwrap();
        let (errors, _messages) = mpsc::unbounded_channel();
        let resources = Resources::new(base.clone(), errors);
        for input in ["", ".", "/", "escape", "../outside"] {
            assert!(
                resources
                    .serve_file(input.to_owned(), 0, false, false)
                    .await
                    .is_err(),
                "{input:?}"
            );
        }
        let handle = resources.reserve().await.unwrap();
        assert!(resources.clear_http(handle).await);
        assert!(!resources.clear_http(handle).await);
        assert_eq!(fs::read(outside).await.unwrap(), b"secret");
    }

    #[tokio::test]
    async fn malformed_requests_do_not_emit_responses_without_an_origin() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, duplex};
        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            let file = dir.path().join("file");
            fs::write(&file, b"abc").await.unwrap();
            let root = Arc::new(ServingRoot::File(file));
            for after_response in [false, true] {
                let (mut client, server) = duplex(8192);
                let server = spawn(serve_stream(
                    server,
                    false,
                    root.clone(),
                    "http://127.0.0.1:1234".to_owned(),
                ));
                if after_response {
                    client
                        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        .await
                        .unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(client.read_u8().await.unwrap());
                    }
                    assert!(
                        String::from_utf8_lossy(&header)
                            .contains("access-control-allow-origin: http://127.0.0.1:1234")
                    );
                    let mut body = [0; 3];
                    client.read_exact(&mut body).await.unwrap();
                    assert_eq!(&body, b"abc");
                }
                client
                    .write_all(b"GET / HTTP/1.1\r\ninvalid header\r\n\r\n")
                    .await
                    .unwrap();
                client.shutdown().await.unwrap();
                let mut response = Vec::new();
                client.read_to_end(&mut response).await.unwrap();
                assert!(
                    response.is_empty(),
                    "{after_response}: {}",
                    String::from_utf8_lossy(&response)
                );
                assert!(server.await.unwrap().is_err());
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rejected_methods_advertise_the_accepted_methods() {
        let root = ServingRoot::Directory(PathBuf::from("/unused"));
        for (http2, allowed) in [(false, "GET"), (true, "GET, HEAD")] {
            let request = Request::builder()
                .method(Method::POST)
                .uri("/")
                .body(())
                .unwrap();
            let ret = response(request, &root, "http://127.0.0.1:1234", http2).await;
            assert_eq!(ret.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(ret.headers()[ALLOW], allowed);
            assert_eq!(
                ret.headers()[ACCESS_CONTROL_ALLOW_ORIGIN],
                "http://127.0.0.1:1234"
            );
            assert!(
                ret.into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .is_empty()
            );
        }
    }
}
