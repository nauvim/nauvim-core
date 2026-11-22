use crate::checkpoint::Store;
use crate::config::local_data_dir;
use crate::{Client, Config, Error};

use nvim_rpc_plugin::api;
use nvim_rpc_ui::{Checkpoint, Limits};

use tokio::sync::mpsc;
use tokio::task::{JoinHandle, spawn, spawn_blocking};
use tokio::time::{Duration, MissedTickBehavior, interval, timeout};
use tokio_util::sync::CancellationToken;

use std::ptr;

const SAVE_INTERVAL: Duration = Duration::from_secs(1);
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Session {
    store: Store,
    key: String,
    token: String,
}

impl Session {
    pub(super) async fn open(
        config: &Config,
        client: &Client,
        alias: Option<&str>,
        remote: bool,
        limits: Limits,
        errors: &mpsc::UnboundedSender<String>,
    ) -> Option<(Self, Option<Checkpoint>)> {
        let kind = if remote { "ssh" } else { "local" };
        let token = match timeout(IDENTITY_TIMEOUT, session_token(client)).await {
            Ok(Ok(token)) => token,
            Ok(Err(e)) => {
                let _ = errors.send(format!("Identify Neovim for UI checkpoint: {e}"));
                return None;
            }
            Err(_) => {
                let _ = errors.send("Identify Neovim for UI checkpoint: timed out".into());
                return None;
            }
        };
        let key = alias.map_or_else(|| "default".into(), |alias| format!("{kind}:{alias}"));
        let data_dir = config
            .client_info()
            .map(|info| info.project_dirs.data_dir.clone());
        let load_errors = errors.clone();
        let result = spawn_blocking(move || {
            let data_dir = match data_dir {
                Some(dir) => dir,
                None => local_data_dir().map_err(|e| e.to_string())?,
            };
            let store =
                Store::open(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
            let checkpoint = match store.load(&key, token.as_bytes(), limits) {
                Ok(checkpoint) => checkpoint,
                Err(e) => {
                    let _ = load_errors.send(format!("Load UI checkpoint for {key}: {e}"));
                    None
                }
            };
            Ok::<_, String>((Self { store, key, token }, checkpoint))
        })
        .await;
        match result {
            Ok(Ok(session)) => Some(session),
            Ok(Err(e)) => {
                let _ = errors.send(format!("Open UI checkpoint database: {e}"));
                None
            }
            Err(e) => {
                let _ = errors.send(format!("Open UI checkpoint database: {e}"));
                None
            }
        }
    }

    pub(super) fn write(
        self,
        checkpoint: impl Fn() -> Option<Checkpoint> + Send + 'static,
        errors: mpsc::UnboundedSender<String>,
    ) -> Writer {
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let mut saved = checkpoint();
        let task = spawn(async move {
            let mut session = self;
            let mut tick = interval(SAVE_INTERVAL);
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut reported_error = false;
            loop {
                let finished = tokio::select! {
                    biased;
                    () = stopped.cancelled() => true,
                    _ = tick.tick() => false,
                };
                if let Some(current) = checkpoint()
                    && saved
                        .as_ref()
                        .is_none_or(|previous| !ptr::eq(current.state(), previous.state()))
                {
                    let result = spawn_blocking(move || {
                        let result =
                            session
                                .store
                                .save(&session.key, session.token.as_bytes(), &current);
                        (session, current, result)
                    })
                    .await;
                    let (next, current, result) = match result {
                        Ok(result) => result,
                        Err(e) => {
                            let _ = errors.send(format!("Save UI checkpoint: {e}"));
                            return;
                        }
                    };
                    session = next;
                    match result {
                        Ok(()) => {
                            saved = Some(current);
                            reported_error = false;
                        }
                        Err(e) if !reported_error => {
                            let _ =
                                errors.send(format!("Save UI checkpoint for {}: {e}", session.key));
                            reported_error = true;
                        }
                        Err(_) => {}
                    }
                }
                if finished {
                    return;
                }
            }
        });
        Writer { stop, task }
    }
}

pub(super) struct Writer {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Writer {
    pub(super) async fn finish(&mut self) {
        self.stop.cancel();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn session_token(client: &Client) -> Result<String, Error> {
    use getrandom::fill;

    // The caller has already selected the owned socket. Match the process on
    // that connection, including hosts started before checkpoint support.
    const SCRIPT: &str = r#"
        local candidate = ...
        local key = 'nauvim_core_session_id'
        local token = rawget(_G, key)
        if token == nil then
            rawset(_G, key, candidate)
            token = candidate
        end
        return token
    "#;

    let mut bytes = [0; 16];
    fill(&mut bytes).map_err(|e| Error::Message(format!("Generate Neovim session ID: {e}")))?;
    let candidate = format!("{:032x}", u128::from_ne_bytes(bytes));
    let token = client
        .nvim()
        .request::<api::NvimExecLua, String>((SCRIPT, (candidate.as_str(),)))
        .await?;
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Message("Invalid Neovim session ID".into()));
    }
    Ok(token)
}
