use crate::Error;

#[cfg(feature = "plugin-http")]
use nvim_rpc_plugin::mp::decode::Array as DecodedArray;
#[cfg(feature = "plugin-http")]
use nvim_rpc_plugin::{Array, Neovim};
use nvim_rpc_plugin::{Connection as RpcConnection, ConnectionError, Handler};
#[cfg(feature = "plugin-http")]
use nvim_rpc_ui::GridId;
use nvim_rpc_ui::{Checkpoint, Client, ClientError, ConnectionOptions, Frames, UiHandler};

use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(feature = "plugin-http")]
use std::collections::BTreeSet;
#[cfg(feature = "plugin-http")]
use std::sync::{Arc, RwLock};

#[cfg(feature = "plugin-http")]
pub(crate) type Grids = Arc<RwLock<BTreeSet<GridId>>>;

pub(crate) struct Connection<W> {
    pub client: Client<W>,
    pub frames: Frames,
    rpc: RpcConnection<W>,
}

impl<W> Connection<W> {
    pub(crate) fn stop(&mut self) {
        self.rpc.stop();
    }

    pub(crate) async fn wait(&mut self) -> Result<(), ConnectionError> {
        self.rpc.wait().await
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> Connection<W> {
    pub(crate) async fn detach_ui(&self) -> Result<(), ClientError> {
        self.client.detach().await
    }
}

pub(crate) fn connect<R, W, H>(
    rdr: R,
    wtr: W,
    handler: H,
    options: ConnectionOptions,
    checkpoint: Option<Checkpoint>,
    #[cfg(feature = "plugin-http")] grids: Grids,
) -> Result<Connection<W>, Error>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    H: Handler<W>,
{
    use nvim_rpc_plugin::connect as connect_rpc;
    use nvim_rpc_ui::Error as UiError;

    let (handler, frames) = match checkpoint {
        Some(checkpoint) => UiHandler::from_checkpoint(handler, checkpoint, options.limits)
            .map_err(|e| Error::Message(format!("Restore UI checkpoint: {e}")))?,
        None => UiHandler::new(handler, options.limits),
    };
    #[cfg(feature = "plugin-http")]
    if let Some(checkpoint) = frames.checkpoint() {
        let mut grids = grids.write().unwrap_or_else(|e| e.into_inner());
        grids.extend(checkpoint.state().grids.keys().copied());
    }
    #[cfg(feature = "plugin-http")]
    let handler = Tracked {
        inner: handler,
        grids,
        max_grids: options.limits.max_grids,
    };
    let rpc = connect_rpc(rdr, wtr, handler, options.rpc).map_err(UiError::from)?;
    let client = Client::new(rpc.client.clone());
    Ok(Connection {
        client,
        frames,
        rpc,
    })
}

#[cfg(feature = "plugin-http")]
struct Tracked<H> {
    inner: UiHandler<H>,
    grids: Grids,
    max_grids: usize,
}

#[cfg(feature = "plugin-http")]
impl<W, H> Handler<W> for Tracked<H>
where
    W: AsyncWrite + Unpin + Send + 'static,
    H: Handler<W>,
{
    type Request = H::Request;
    type Notification = Array;
    type Response = H::Response;

    async fn on_request(
        &self,
        method: &[u8],
        params: Self::Request,
        nvim: Neovim<W>,
    ) -> Self::Response {
        self.inner.on_request(method, params, nvim).await
    }

    async fn on_notification(&self, method: &[u8], params: Array, nvim: Neovim<W>) {
        if method == b"redraw" {
            observe(&self.grids, params.with_borrowed_buffer(), self.max_grids);
        }
        // UiHandler validates the complete notification and closes malformed
        // streams before dispatching later plugin calls. Its model and bounded
        // coalesced frame receiver remain the only owners of rendering state.
        self.inner.on_notification(method, params, nvim).await;
    }
}

#[cfg(feature = "plugin-http")]
fn observe(grids: &Grids, params: DecodedArray<&[u8]>, max_grids: usize) {
    use nvim_rpc_plugin::mp::decode::Object;
    use nvim_rpc_plugin::{Decode, sys::types};

    for event in params {
        let Object::Array(mut event) = event else {
            continue;
        };
        let Some(Object::Str(name)) = event.next() else {
            continue;
        };
        let create = match name.as_slice() {
            b"grid_resize" => true,
            b"grid_destroy" => false,
            _ => continue,
        };
        let mut grids = grids.write().unwrap_or_else(|e| e.into_inner());
        for args in event {
            let Object::Array(mut args) = args else {
                continue;
            };
            let Some(grid) = args.next() else {
                continue;
            };
            let Ok(grid) = <i64 as Decode<types::Object, &[u8]>>::decode(grid) else {
                continue;
            };
            if create {
                // The UI model rejects additional grids at this same bound.
                // Limit the observer too, before it delegates validation.
                if grids.len() < max_grids {
                    grids.insert(GridId(grid));
                }
            } else {
                grids.remove(&GridId(grid));
            }
        }
    }
}

#[cfg(all(test, feature = "plugin-http"))]
mod tests {
    use super::*;

    use nvim_rpc_plugin::mp::decode::{Object, ObjectParser};
    use nvim_rpc_plugin::mp::encode::WriterSyncExt as _;
    use nvim_rpc_plugin::{Array, Bytes};

    fn params(bytes: &[u8]) -> Array {
        let mut parser = ObjectParser::new(Bytes::copy_from_slice(bytes));
        let Object::Array(params) = parser.next_object().unwrap() else {
            panic!("Expected redraw parameters");
        };
        params
    }

    #[tokio::test]
    async fn tracks_grid_lifetimes_before_the_frontend_receives_frames() {
        use nvim_rpc_plugin::connect as connect_rpc;
        use nvim_rpc_ui::NoHandler;

        use tokio::io::{duplex, split};

        let grids = Grids::default();
        let options = ConnectionOptions::default();
        let (inner, _unpolled_frames) = UiHandler::new(NoHandler, options.limits);
        let handler = Tracked {
            inner,
            grids: grids.clone(),
            max_grids: options.limits.max_grids,
        };
        let (stream, _peer) = duplex(4096);
        let (rdr, wtr) = split(stream);
        let mut rpc = connect_rpc(rdr, wtr, NoHandler, options.rpc).unwrap();
        let mut bytes = Vec::new();
        bytes
            .write_primitive((("grid_resize", (1, 3, 2), (2, 2, 2)), ("flush", ())))
            .consume();
        handler
            .on_notification(b"redraw", params(&bytes), rpc.client.clone())
            .await;
        assert_eq!(
            grids.read().unwrap().iter().copied().collect::<Vec<_>>(),
            [GridId(1), GridId(2)]
        );

        bytes.clear();
        bytes
            .write_primitive((("grid_destroy", (2,)), ("flush", ())))
            .consume();
        handler
            .on_notification(b"redraw", params(&bytes), rpc.client.clone())
            .await;
        assert_eq!(
            grids.read().unwrap().iter().copied().collect::<Vec<_>>(),
            [GridId(1)]
        );
        rpc.close_and_wait().await.unwrap();
    }

    fn checkpoint_with_hidden_grid() -> Checkpoint {
        use nvim_rpc_ui::Model;

        let mut bytes = Vec::new();
        bytes
            .write_primitive((
                ("grid_resize", (1, 3, 2), (2, 2, 2)),
                ("win_hide", (2,)),
                ("flush", ()),
            ))
            .consume();
        let redraw = params(&bytes);
        let mut model = Model::default();
        model
            .apply_notification(b"redraw", redraw.with_borrowed_buffer(), |_| {})
            .unwrap();
        model.checkpoint().unwrap()
    }

    #[tokio::test]
    async fn restores_plugin_grid_ids_before_the_first_redraw() {
        use nvim_rpc_ui::NoHandler;

        use tokio::io::{duplex, split};

        let grids = Grids::default();
        let (stream, _peer) = duplex(4096);
        let (rdr, wtr) = split(stream);
        let mut connection = connect(
            rdr,
            wtr,
            NoHandler,
            ConnectionOptions::default(),
            Some(checkpoint_with_hidden_grid()),
            Arc::clone(&grids),
        )
        .unwrap();

        assert_eq!(
            grids.read().unwrap().iter().copied().collect::<Vec<_>>(),
            [GridId(1), GridId(2)]
        );
        connection.stop();
        connection.wait().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_checkpoints_outside_the_connection_limits() {
        use nvim_rpc_ui::NoHandler;

        use tokio::io::{duplex, split};

        let mut options = ConnectionOptions::default();
        options.limits.max_grids = 1;
        let grids = Grids::default();
        let (stream, _peer) = duplex(4096);
        let (rdr, wtr) = split(stream);
        let result = connect(
            rdr,
            wtr,
            NoHandler,
            options,
            Some(checkpoint_with_hidden_grid()),
            Arc::clone(&grids),
        );

        assert!(matches!(
            result,
            Err(Error::Message(message)) if message.starts_with("Restore UI checkpoint: ")
        ));
        assert!(grids.read().unwrap().is_empty());
    }

    #[test]
    fn bounds_observed_grids_before_protocol_validation() {
        let grids = Grids::default();
        let mut bytes = Vec::new();
        bytes
            .write_primitive((("grid_resize", (1, 1, 1), (2, 1, 1), (3, 1, 1)),))
            .consume();
        observe(&grids, params(&bytes).with_borrowed_buffer(), 2);
        assert_eq!(
            grids.read().unwrap().iter().copied().collect::<Vec<_>>(),
            [GridId(1), GridId(2)]
        );
    }
}
