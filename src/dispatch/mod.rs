#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http",
    feature = "plugin-clipboard"
))]
use crate::Policies;
#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
use crate::ResourceHandle;
#[cfg(feature = "plugin-http")]
use crate::connection::Grids;
#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
use crate::resources::Resources;
#[cfg(feature = "plugin-clipboard")]
use crate::resources::clipboard::Clipboard;
use crate::{ClientId, EngineEvent};

#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
use nvim_rpc_plugin::mp::encode::Untagged;
use nvim_rpc_plugin::mp::encode::primitive::Nil;
use nvim_rpc_plugin::sys::types;
use nvim_rpc_plugin::{Array, Bytes, Decode, Handler, Neovim};

use tokio::io::AsyncWrite;
use tokio::sync::mpsc;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
type ReplyValue = Untagged<Nil, i64>;
#[cfg(not(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
)))]
type ReplyValue = Nil;

pub(crate) struct Dispatch {
    pub client_id: ClientId,
    pub host_quit: Arc<AtomicBool>,
    pub host_detach: Arc<AtomicBool>,
    #[cfg(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http"
    ))]
    pub resources: Arc<Resources>,
    pub events: mpsc::UnboundedSender<EngineEvent>,
    #[cfg(feature = "plugin-http")]
    pub grids: Grids,
    #[cfg(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http",
        feature = "plugin-clipboard"
    ))]
    pub policies: Policies,
    #[cfg(feature = "plugin-clipboard")]
    pub clipboard: Arc<Clipboard>,
    #[cfg(feature = "plugin-clipboard")]
    pub clipboard_errors: mpsc::UnboundedSender<String>,
}

impl Dispatch {
    async fn request(&self, method: &[u8], mut params: Array) -> Result<ReplyValue, String> {
        // Decode the ID first, including before method recognition and arity checks.
        if !is_addressed_to(&mut params, self.client_id) {
            return Ok(no_effect());
        }
        validate_params(&params)?;
        if let Some(flag) = match method {
            b"nauvim-plugin/client.host_quit" => Some(&self.host_quit),
            b"nauvim-plugin/client.host_detach" => Some(&self.host_detach),
            _ => None,
        } {
            end(&params)?;
            flag.store(true, Ordering::Release);
            return Ok(no_effect());
        }
        match method {
            #[cfg(feature = "plugin-local-forward")]
            b"nauvim-plugin/local-forward.open_tcp" => {
                allowed(self.policies.local_forward)?;
                let port = next(&mut params)?;
                let addr = next(&mut params)?;
                let host_port = next(&mut params)?;
                end(&params)?;
                Ok(handle_reply(
                    self.resources.open_tcp(port, addr, host_port).await?,
                ))
            }
            #[cfg(feature = "plugin-local-forward")]
            b"nauvim-plugin/local-forward.open_socket" => {
                allowed(self.policies.local_forward)?;
                let client = next(&mut params)?;
                let host = next(&mut params)?;
                end(&params)?;
                Ok(handle_reply(
                    self.resources.open_socket(client, host).await?,
                ))
            }
            #[cfg(feature = "plugin-local-forward")]
            b"nauvim-plugin/local-forward.get_port" => {
                allowed(self.policies.local_forward)?;
                let handle = ResourceHandle::from(next::<i64>(&mut params)?);
                end(&params)?;
                Ok(self
                    .resources
                    .get_port(handle)
                    .await?
                    .map_or_else(no_effect, |port| integer_reply(i64::from(port))))
            }
            #[cfg(feature = "plugin-filesystem")]
            b"nauvim-plugin/filesystem.copy" => {
                allowed(self.policies.filesystem)?;
                let client = next(&mut params)?;
                let host = next(&mut params)?;
                let overwrite = next(&mut params)?;
                end(&params)?;
                Ok(handle_reply(
                    self.resources.copy(client, host, overwrite).await?,
                ))
            }
            #[cfg(feature = "plugin-http")]
            b"nauvim-plugin/http.serve_file" => {
                allowed(self.policies.http)?;
                let file = next(&mut params)?;
                let port = next(&mut params)?;
                let https = next(&mut params)?;
                let http2 = next(&mut params)?;
                end(&params)?;
                Ok(handle_reply(
                    self.resources.serve_file(file, port, https, http2).await?,
                ))
            }
            #[cfg(feature = "plugin-http")]
            b"nauvim-plugin/http.get_port" => {
                allowed(self.policies.http)?;
                let handle = ResourceHandle::from(next::<i64>(&mut params)?);
                end(&params)?;
                Ok(self
                    .resources
                    .get_http_port(handle)
                    .await?
                    .map_or_else(no_effect, |port| integer_reply(i64::from(port))))
            }
            #[cfg(feature = "plugin-http")]
            b"nauvim-plugin/http.open_webview" => {
                use crate::{GridId, WebviewPosition};

                use nvim_rpc_plugin::Map;

                use url::Url;

                allowed(self.policies.http)?;
                let url: String = next(&mut params)?;
                let parsed = Url::parse(&url).map_err(|e| e.to_string())?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
                    return Err("Webview URL must use http or https".into());
                }
                let mut map: Map = next(&mut params)?;
                end(&params)?;
                let mut grid = None;
                let mut top = None;
                let mut left = None;
                let mut height = None;
                let mut width = None;
                for pair in &mut map {
                    let (key, value) = (pair.key, pair.value);
                    let key = <String as Decode<types::Object, Bytes>>::decode(key)
                        .map_err(|e| e.to_string())?;
                    match key.as_str() {
                        "grid" => {
                            if grid.is_some() {
                                return Err("Duplicate position.grid".into());
                            }
                            grid = Some(
                                <i64 as Decode<types::Object, Bytes>>::decode(value)
                                    .map_err(|e| e.to_string())?,
                            )
                        }
                        "top" => {
                            if top.is_some() {
                                return Err("Duplicate position.top".into());
                            }
                            top = Some(
                                <u16 as Decode<types::Object, Bytes>>::decode(value)
                                    .map_err(|e| e.to_string())?,
                            )
                        }
                        "left" => {
                            if left.is_some() {
                                return Err("Duplicate position.left".into());
                            }
                            left = Some(
                                <u16 as Decode<types::Object, Bytes>>::decode(value)
                                    .map_err(|e| e.to_string())?,
                            )
                        }
                        "height" => {
                            if height.is_some() {
                                return Err("Duplicate position.height".into());
                            }
                            height = Some(
                                <u16 as Decode<types::Object, Bytes>>::decode(value)
                                    .map_err(|e| e.to_string())?,
                            )
                        }
                        "width" => {
                            if width.is_some() {
                                return Err("Duplicate position.width".into());
                            }
                            width = Some(
                                <u16 as Decode<types::Object, Bytes>>::decode(value)
                                    .map_err(|e| e.to_string())?,
                            )
                        }
                        _ => return Err("Unknown Webview position field".into()),
                    }
                }
                let position = WebviewPosition {
                    grid: GridId(grid.ok_or("Missing position.grid")?),
                    top: top.ok_or("Missing position.top")?,
                    left: left.ok_or("Missing position.left")?,
                    height: height.ok_or("Missing position.height")?,
                    width: width.ok_or("Missing position.width")?,
                };
                let handle = self.resources.reserve().await?;
                if self
                    .grids
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&position.grid)
                {
                    let _ = self.events.send(EngineEvent::OpenWebview {
                        handle,
                        url,
                        position,
                    });
                }
                Ok(handle_reply(handle))
            }
            _ => Err("Unknown or disabled Nauvim request".into()),
        }
    }

    async fn notification(&self, method: &[u8], mut params: Array) -> Result<(), String> {
        if !is_addressed_to(&mut params, self.client_id) {
            return Ok(());
        }
        validate_params(&params)?;
        match method {
            b"nauvim-plugin/client.connect" => {
                let host = next(&mut params)?;
                end(&params)?;
                let _ = self.events.send(EngineEvent::Connect { host });
            }
            #[cfg(feature = "plugin-clipboard")]
            b"nauvim-plugin/clipboard.copy" => {
                allowed(self.policies.clipboard)?;
                let register: String = next(&mut params)?;
                if !matches!(register.as_str(), "+" | "*") {
                    return Err("Clipboard register must be + or *".into());
                }
                let text: String = next(&mut params)?;
                end(&params)?;
                let errors = self.clipboard_errors.clone();
                self.clipboard.copy_later(text, register, move |e| {
                    let _ = errors.send(e);
                })?;
            }
            #[cfg(feature = "plugin-local-forward")]
            b"nauvim-plugin/local-forward.clear" => {
                let h = ResourceHandle::from(next::<i64>(&mut params)?);
                end(&params)?;
                self.resources.clear_forward(h).await;
            }
            #[cfg(feature = "plugin-filesystem")]
            b"nauvim-plugin/filesystem.clear" => {
                let h = ResourceHandle::from(next::<i64>(&mut params)?);
                end(&params)?;
                self.resources.clear_copy(h).await;
            }
            #[cfg(feature = "plugin-http")]
            b"nauvim-plugin/http.clear" => {
                let handle = ResourceHandle::from(next::<i64>(&mut params)?);
                end(&params)?;
                if self.resources.clear_http(handle).await {
                    let _ = self.events.send(EngineEvent::CloseWebview { handle });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> Handler<W> for Dispatch {
    type Request = Array;
    type Notification = Array;
    type Response = Result<ReplyValue, String>;

    async fn on_request(&self, method: &[u8], params: Array, _: Neovim<W>) -> Self::Response {
        self.request(method, params).await.map_err(wire_error)
    }
    async fn on_notification(&self, method: &[u8], params: Array, _: Neovim<W>) {
        if let Err(e) = self.notification(method, params).await {
            let _ = self.events.send(EngineEvent::Error(e));
        }
    }
}

fn wire_error(s: String) -> String {
    if s.starts_with('\u{feff}') || s.contains('\0') {
        s.chars()
            .enumerate()
            .map(|(idx, c)| {
                if c == '\0' || idx == 0 && c == '\u{feff}' {
                    '\u{fffd}'
                } else {
                    c
                }
            })
            .collect()
    } else if s.is_empty() {
        "Nauvim request failed".into()
    } else {
        s
    }
}

fn validate_params(params: &Array) -> Result<(), String> {
    use nvim_rpc_plugin::mp::decode::Object;

    fn check(value: Object<&[u8]>, remaining: u8) -> Result<(), String> {
        let remaining = remaining
            .checked_sub(1)
            .ok_or("RPC arguments are nested too deeply")?;
        match value {
            Object::Str(s) | Object::Bin(s) => {
                let s = s.as_str().map_err(|_| "RPC strings must be valid UTF-8")?;
                if s.starts_with('\u{feff}') || s.contains('\0') {
                    return Err("RPC strings must not begin with U+FEFF or contain NUL".into());
                }
            }
            Object::Array(values) => {
                for value in values {
                    check(value, remaining)?;
                }
            }
            Object::Map(values) => {
                for pair in values {
                    check(pair.key, remaining)?;
                    check(pair.value, remaining)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    for value in params.with_borrowed_buffer() {
        // Supported arguments are scalars or position dictionaries.
        // Bound rejected shapes too, without growing the stack
        // in proportion to an untrusted message's nesting depth.
        check(value, 8)?;
    }
    Ok(())
}

fn next<T: Decode<types::Object, Bytes>>(params: &mut Array) -> Result<T, String> {
    T::decode(params.next().ok_or("Missing RPC argument")?).map_err(|e| e.to_string())
}

fn is_addressed_to(params: &mut Array, client_id: ClientId) -> bool {
    params
        .next()
        .and_then(|value| <u32 as Decode<types::Object, Bytes>>::decode(value).ok())
        == Some(client_id.get())
}
fn end(params: &Array) -> Result<(), String> {
    if params.remaining_len() == 0 {
        Ok(())
    } else {
        Err("Too many RPC arguments".into())
    }
}
#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http",
    feature = "plugin-clipboard"
))]
fn allowed(value: bool) -> Result<(), String> {
    if value {
        Ok(())
    } else {
        Err("Client policy denies this operation".into())
    }
}

fn no_effect() -> ReplyValue {
    #[cfg(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http"
    ))]
    {
        Untagged::Left(Nil)
    }
    #[cfg(not(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http"
    )))]
    {
        Nil
    }
}

#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
fn handle_reply(handle: ResourceHandle) -> ReplyValue {
    integer_reply(handle.get())
}

#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
fn integer_reply(value: i64) -> ReplyValue {
    Untagged::Right(value)
}

#[cfg(test)]
mod tests;
