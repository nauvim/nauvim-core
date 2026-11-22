use crate::Error;

#[cfg(feature = "plugin-http")]
use nvim_rpc_plugin::mp::decode::Array as DecodedArray;
use nvim_rpc_plugin::{Array, Connection as RpcConnection, ConnectionError, Handler, Neovim};
#[cfg(feature = "plugin-http")]
use nvim_rpc_ui::GridId;
use nvim_rpc_ui::{
    Checkpoint, CheckpointError, Client, ClientError, ConnectionOptions, Error as UiError, Frame,
    FrameError, Frames, Limits, NoHandler, UiHandler,
};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;

#[cfg(feature = "plugin-http")]
use std::collections::BTreeSet;
#[cfg(feature = "plugin-http")]
use std::sync::RwLock;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "plugin-http")]
pub(crate) type Grids = Arc<RwLock<BTreeSet<GridId>>>;

pub(crate) struct Connection<W> {
    pub client: Client<W>,
    frames: Option<FrameReceiver>,
    rpc: RpcConnection<W>,
}

impl<W> Connection<W> {
    pub(crate) fn initialize_ui(
        &mut self,
        setup: UiSetup,
        checkpoint: Option<Checkpoint>,
    ) -> Option<CheckpointError> {
        let (frames, error) = setup.initialize(checkpoint);
        self.frames = Some(frames);
        error
    }

    pub(crate) fn checkpoint_source(&self) -> impl Fn() -> Option<Checkpoint> + Send + 'static {
        let frames = self
            .frames
            .as_ref()
            .map(|receiver| Arc::clone(&receiver.frames));
        move || {
            let frames = frames.as_ref()?;
            frames
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .checkpoint()
        }
    }

    pub(crate) async fn recv_frame(&mut self) -> Option<Result<Frame, FrameError>> {
        self.frames.as_mut()?.recv().await
    }

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

pub(crate) fn begin<R, W, H>(
    rdr: R,
    wtr: W,
    handler: H,
    options: ConnectionOptions,
    #[cfg(feature = "plugin-http")] grids: Grids,
) -> Result<(Connection<W>, UiSetup), Error>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    H: Handler<W, Notification = Array>,
{
    use nvim_rpc_plugin::connect as connect_rpc;

    let ui = Arc::new(ReceivingUi {
        handler: OnceLock::new(),
        ready: Arc::new(Notify::new()),
        limits: options.limits,
        #[cfg(feature = "plugin-http")]
        grids,
    });
    let handler = DispatchUi {
        handler,
        ui: Arc::clone(&ui),
    };
    let rpc = connect_rpc(rdr, wtr, handler, options.rpc).map_err(UiError::from)?;
    let client = Client::new(rpc.client.clone());
    Ok((
        Connection {
            client,
            frames: None,
            rpc,
        },
        UiSetup(ui),
    ))
}

pub(crate) struct UiSetup(Arc<ReceivingUi>);

impl UiSetup {
    fn initialize(
        self,
        checkpoint: Option<Checkpoint>,
    ) -> (FrameReceiver, Option<CheckpointError>) {
        let ui = self.0;
        let mut error = None;
        let (handler, frames) = match checkpoint {
            Some(checkpoint) => {
                match UiHandler::from_checkpoint(NoHandler, checkpoint, ui.limits) {
                    Ok(restored) => restored,
                    Err(e) => {
                        error = Some(e);
                        UiHandler::new(NoHandler, ui.limits)
                    }
                }
            }
            None => UiHandler::new(NoHandler, ui.limits),
        };
        #[cfg(feature = "plugin-http")]
        if let Some(checkpoint) = frames.checkpoint() {
            let mut grids = ui.grids.write().unwrap_or_else(|e| e.into_inner());
            grids.extend(checkpoint.state().grids.keys().copied());
        }
        let _ = ui.handler.set(handler);
        let receiver = FrameReceiver {
            frames: Arc::new(Mutex::new(frames)),
            ready: Arc::clone(&ui.ready),
        };
        (receiver, error)
    }
}

struct ReceivingUi {
    handler: OnceLock<UiHandler<NoHandler>>,
    ready: Arc<Notify>,
    limits: Limits,
    #[cfg(feature = "plugin-http")]
    grids: Grids,
}

impl Drop for ReceivingUi {
    fn drop(&mut self) {
        drop(self.handler.take());
        self.ready.notify_one();
    }
}

struct DispatchUi<H> {
    handler: H,
    ui: Arc<ReceivingUi>,
}

impl<W, H> Handler<W> for DispatchUi<H>
where
    W: AsyncWrite + Unpin + Send + 'static,
    H: Handler<W, Notification = Array>,
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
        self.handler.on_request(method, params, nvim).await
    }

    async fn on_notification(&self, method: &[u8], params: Array, nvim: Neovim<W>) {
        if !matches!(method, b"redraw" | b"nvim_error_event") {
            self.handler.on_notification(method, params, nvim).await;
            return;
        }
        let Some(ui) = self.ui.handler.get() else {
            return;
        };
        #[cfg(feature = "plugin-http")]
        if method == b"redraw" {
            observe(
                &self.ui.grids,
                params.with_borrowed_buffer(),
                self.ui.limits.max_grids,
            );
        }
        ui.on_notification(method, params, nvim).await;
        self.ui.ready.notify_one();
    }
}

struct FrameReceiver {
    frames: Arc<Mutex<Frames>>,
    ready: Arc<Notify>,
}

impl FrameReceiver {
    async fn recv(&mut self) -> Option<Result<Frame, FrameError>> {
        use std::future::Future as _;
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};

        loop {
            let ready = self.ready.notified();
            {
                // Poll once so checkpoint sampling can borrow Frames while the
                // frontend waits. DispatchUi and ReceivingUi supply the wakeups
                // for publication, protocol errors, and handler shutdown.
                let mut frames = self.frames.lock().unwrap_or_else(|e| e.into_inner());
                let recv = pin!(frames.recv());
                if let Poll::Ready(result) = recv.poll(&mut Context::from_waker(Waker::noop())) {
                    return result;
                }
            }
            ready.await;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "plugin-http")]
    use nvim_rpc_plugin::mp::decode::{Object, ObjectParser};
    #[cfg(feature = "plugin-http")]
    use nvim_rpc_plugin::mp::encode::WriterSyncExt as _;
    #[cfg(feature = "plugin-http")]
    use nvim_rpc_plugin::{Array, Bytes};

    #[tokio::test]
    async fn frame_delivery_stops_while_a_client_send_holds_the_writer() {
        use nvim_rpc_plugin::{Object, api};

        use tokio::io::{AsyncReadExt as _, duplex, split};
        use tokio::time::{Duration, timeout};

        use std::pin::pin;

        let (stream, mut peer) = duplex(64);
        let (rdr, wtr) = split(stream);
        let (mut rpc, ui) = begin(
            rdr,
            wtr,
            NoHandler,
            ConnectionOptions::default(),
            #[cfg(feature = "plugin-http")]
            Grids::default(),
        )
        .unwrap();
        assert!(rpc.initialize_ui(ui, None).is_none());

        {
            let client = rpc.client.clone();
            let command = "x".repeat(4096);
            let mut sending = pin!(
                client
                    .nvim()
                    .request::<api::NvimCommand, Object>((command.as_str(),))
            );
            let mut first_byte = [0];
            // Observe a partial write with plenty of data left to send.
            tokio::select! {
                biased;
                result = &mut sending => panic!("send finished early: {result:?}"),
                read = timeout(Duration::from_secs(1), peer.read_exact(&mut first_byte)) => {
                    read.unwrap().unwrap();
                }
            }

            rpc.stop();
            assert!(
                timeout(Duration::from_secs(1), rpc.recv_frame())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                timeout(Duration::from_millis(20), rpc.wait())
                    .await
                    .is_err()
            );
        }
        timeout(Duration::from_secs(1), rpc.wait())
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(feature = "plugin-http")]
    fn params(bytes: &[u8]) -> Array {
        let mut parser = ObjectParser::new(Bytes::copy_from_slice(bytes));
        let Object::Array(params) = parser.next_object().unwrap() else {
            panic!("Expected redraw parameters");
        };
        params
    }

    #[cfg(feature = "plugin-http")]
    #[tokio::test]
    async fn tracks_grid_lifetimes_before_the_frontend_receives_frames() {
        use nvim_rpc_plugin::connect as connect_rpc;
        use nvim_rpc_ui::NoHandler;

        use tokio::io::{duplex, split};

        let grids = Grids::default();
        let options = ConnectionOptions::default();
        let ui = Arc::new(ReceivingUi {
            handler: OnceLock::new(),
            ready: Arc::new(Notify::new()),
            limits: options.limits,
            grids: grids.clone(),
        });
        let (_unpolled_frames, error) = UiSetup(Arc::clone(&ui)).initialize(None);
        assert!(error.is_none());
        let handler = DispatchUi {
            handler: NoHandler,
            ui,
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

    #[cfg(feature = "plugin-http")]
    #[test]
    fn restores_plugin_grid_ids_before_the_first_redraw() {
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

        let grids = Grids::default();
        let setup = UiSetup(Arc::new(ReceivingUi {
            handler: OnceLock::new(),
            ready: Arc::new(Notify::new()),
            limits: Limits::default(),
            grids: grids.clone(),
        }));
        let (_unpolled_frames, error) = setup.initialize(model.checkpoint());

        assert!(error.is_none());
        assert_eq!(
            grids.read().unwrap().iter().copied().collect::<Vec<_>>(),
            [GridId(1), GridId(2)]
        );
    }

    #[cfg(feature = "plugin-http")]
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
