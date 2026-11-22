use crate::resources::Registry;
#[cfg(feature = "ssh")]
use crate::resources::Remote;
#[cfg(feature = "plugin-clipboard")]
use crate::resources::clipboard::Clipboard;
use crate::{ClientId, Config, Error, Policies, Transport};

use super::snapshot::Session;
use super::{ATTACH_TIMEOUT, Attachment, CHECK_TIMEOUT, EngineEvent};

use tokio::fs;
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

pub(super) struct Attempt {
    pub(super) config: Arc<Config>,
    pub(super) client_id: ClientId,
    pub(super) temp_path: PathBuf,
    pub(super) errors_tx: mpsc::UnboundedSender<String>,
    pub(super) cleanup_events_tx: mpsc::UnboundedSender<EngineEvent>,
    pub(super) handles: Arc<AtomicI64>,
    pub(super) registry: Arc<Registry>,
    pub(super) attachment_gate: Arc<Mutex<()>>,
    pub(super) active: Option<Attachment>,
    #[cfg(feature = "plugin-clipboard")]
    pub(super) clipboard: Arc<Clipboard>,
}

impl Attempt {
    pub(super) async fn run(
        mut self,
        alias: Option<String>,
        width: u32,
        height: u32,
        stop: CancellationToken,
    ) -> Result<Attachment, Error> {
        let result = tokio::select! {
            () = stop.cancelled() => Err(Error::Message("Session attachment cancelled".into())),
            result = timeout(ATTACH_TIMEOUT, self.connect_inner(alias.as_deref(), width, height)) =>
                result.unwrap_or_else(|_| Err(Error::Timeout("Session attachment"))),
        };
        match result {
            Ok(()) => self
                .active
                .take()
                .ok_or_else(|| Error::Message("Attachment disappeared".into())),
            Err(e) => {
                if let Some(active) = self.active.take() {
                    active.cleanup(false, self.cleanup_events_tx.clone()).await;
                }
                Err(e)
            }
        }
    }

    async fn connect_inner(
        &mut self,
        alias: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<(), Error> {
        use crate::connection;
        #[cfg(feature = "plugin-http")]
        use crate::connection::Grids;
        use crate::dispatch::Dispatch;
        use crate::resources::Resources;

        use nvim_rpc_ui::ConnectionOptions;

        use tokio::io::split;

        use std::sync::atomic::AtomicBool;

        #[cfg(feature = "ssh")]
        let is_remote =
            alias.is_some_and(|alias| self.config.hosts.get(alias).is_none_or(|h| h.ssh.is_some()));
        #[cfg(not(feature = "ssh"))]
        let is_remote =
            alias.is_some_and(|alias| self.config.hosts.get(alias).is_none_or(|h| h.remote));
        let attachment_guard = Arc::clone(&self.attachment_gate).lock_owned().await;
        let (transport, _policies, _remote) = if is_remote {
            #[cfg(feature = "ssh")]
            {
                use crate::ssh;

                let alias =
                    alias.ok_or_else(|| Error::Message("Missing remote host alias".into()))?;
                let (transport, remote) = ssh::open(&self.config, alias).await?;
                let policies = self
                    .config
                    .hosts
                    .get(alias)
                    .map(|h| h.policies.clone())
                    .unwrap_or_default();
                (transport, policies, Some(remote))
            }
            #[cfg(not(feature = "ssh"))]
            {
                return Err(Error::Message(
                    "SSH support is disabled in this build".into(),
                ));
            }
        } else {
            let (transport, policies) = self.open_local(alias).await?;
            (transport, policies, None::<RemoteMarker>)
        };
        let (events_tx, events) = mpsc::unbounded_channel();
        let resources = Resources::with_registry(
            self.temp_path.clone(),
            self.errors_tx.clone(),
            Arc::clone(&self.handles),
            Arc::clone(&self.registry),
        );
        #[cfg(feature = "ssh")]
        let resources = {
            let mut resources = resources;
            if let Some(remote) = &_remote {
                resources.set_remote(Arc::clone(remote));
            }
            resources
        };
        let resources = Arc::new(resources);
        #[cfg(feature = "plugin-http")]
        let grids = Grids::default();
        let host_quit = Arc::new(AtomicBool::new(false));
        let host_detach = Arc::new(AtomicBool::new(false));
        let handler = Dispatch {
            host_quit: Arc::clone(&host_quit),
            host_detach: Arc::clone(&host_detach),
            client_id: self.client_id,
            #[cfg(any(
                feature = "plugin-local-forward",
                feature = "plugin-filesystem",
                feature = "plugin-http"
            ))]
            resources: Arc::clone(&resources),
            events: events_tx,
            #[cfg(feature = "plugin-http")]
            grids: Arc::clone(&grids),
            #[cfg(any(
                feature = "plugin-local-forward",
                feature = "plugin-filesystem",
                feature = "plugin-http",
                feature = "plugin-clipboard"
            ))]
            policies: _policies,
            #[cfg(feature = "plugin-clipboard")]
            clipboard: Arc::clone(&self.clipboard),
            #[cfg(feature = "plugin-clipboard")]
            clipboard_errors: self.errors_tx.clone(),
        };
        let (reader, writer) = split(transport);
        let options = ConnectionOptions::default();
        let (rpc, ui) = connection::begin(
            reader,
            writer,
            handler,
            options,
            #[cfg(feature = "plugin-http")]
            grids,
        )?;
        // Store before any awaited registration so timeout/cancellation can close it.
        self.active = Some(Attachment {
            rpc,
            checkpoint_writer: None,
            attachment_guard: Some(attachment_guard),
            resources,
            events,
            setup: None,
            host_quit,
            host_detach,
            #[cfg(feature = "plugin-clipboard")]
            clipboard: Arc::clone(&self.clipboard),
            #[cfg(feature = "ssh")]
            remote: _remote,
        });
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| Error::Message("Attachment disappeared".into()))?;
        let session = Session::open(
            &self.config,
            &active.rpc.client,
            alias,
            is_remote,
            options.limits,
            &self.errors_tx,
        )
        .await;
        let (session, checkpoint) = session.unzip();
        if let Some(e) = active.rpc.initialize_ui(ui, checkpoint.flatten()) {
            let _ = self.errors_tx.send(format!("Restore UI checkpoint: {e}"));
        }
        if let Some(session) = session {
            active.checkpoint_writer =
                Some(session.write(active.rpc.checkpoint_source(), self.errors_tx.clone()));
        }
        self.register(width, height).await
    }

    pub(super) async fn register(&mut self, width: u32, height: u32) -> Result<(), Error> {
        use nvim_rpc_plugin::mp::encode::primitive::Map;
        use nvim_rpc_plugin::{Object, api};

        use tokio::spawn;

        let active = self
            .active
            .as_mut()
            .ok_or_else(|| Error::Message("Attachment disappeared".into()))?;
        let name = format!("nauvim-client-{}", self.client_id);
        let mut version = [("major", 0i64), ("minor", 0), ("patch", 0)];
        for ((_, value), part) in version.iter_mut().zip([
            env!("CARGO_PKG_VERSION_MAJOR"),
            env!("CARGO_PKG_VERSION_MINOR"),
            env!("CARGO_PKG_VERSION_PATCH"),
        ]) {
            *value = part
                .parse()
                .map_err(|e| Error::Message(format!("Invalid core package version: {e}")))?;
        }
        let registered = active
            .rpc
            .client
            .nvim()
            .request::<api::NvimSetClientInfo, Object>((
                name.as_str(),
                Map(version),
                "ui",
                Map([] as [(&str, i64); 0]),
                Map([("license", env!("CARGO_PKG_LICENSE"))]),
            ))
            .await
            .is_ok();
        {
            use nvim_rpc_plugin::mp::encode::Untagged as Value;

            let options = Map([
                ("rgb", Value::Left(true)),
                ("override", Value::Left(true)),
                ("ext_cmdline", Value::Left(true)),
                ("ext_popupmenu", Value::Left(true)),
                ("ext_tabline", Value::Left(true)),
                ("ext_messages", Value::Left(true)),
                ("ext_linegrid", Value::Left(true)),
                ("ext_multigrid", Value::Left(true)),
                ("ext_hlstate", Value::Left(true)),
                ("ext_termcolors", Value::Left(true)),
                ("term_name", Value::Right(Value::Left("nauvim"))),
                ("term_colors", Value::Right(Value::Right(16_777_216_u32))),
                ("stdin_tty", Value::Left(false)),
                ("stdout_tty", Value::Left(false)),
            ]);
            active
                .rpc
                .client
                .nvim()
                .request::<api::NvimUiAttach, Object>((
                    i64::from(width),
                    i64::from(height),
                    options,
                ))
                .await?;
        }
        // Lifecycle observation is independent of plugin registration. A failed
        // client-information call must leave ordinary UI operation available.
        if let Ok((channel, _)) = active
            .rpc
            .client
            .nvim()
            .request::<api::NvimGetApiInfo, (i64, Object)>(())
            .await
        {
            let lifecycle = format!(
                r#"
            local chan = {channel}
            local group = vim.api.nvim_create_augroup('NauvimLifecycle' .. chan, {{clear=true}})
            vim.api.nvim_create_autocmd('VimLeavePre', {{group=group, callback=function()
                pcall(vim.rpcrequest, chan, 'nauvim-plugin/client.host_quit', {})
            end}})
            vim.api.nvim_create_autocmd('UILeave', {{group=group, callback=function()
                if vim.v.event.chan == chan then
                    pcall(vim.rpcrequest, chan, 'nauvim-plugin/client.host_detach', {})
                    pcall(vim.api.nvim_del_augroup_by_id, group)
                end
            end}})
        "#,
                self.client_id, self.client_id
            );
            let _ = active
                .rpc
                .client
                .nvim()
                .request::<api::NvimExecLua, Object>((lifecycle.as_str(), ()))
                .await;
        }
        if !registered {
            return Ok(());
        }
        // The integer literal is intentional; host setup derives the exact client name.
        let src = format!(
            "vim.schedule(function() pcall(function() require('nauvim-plugin.client').setup({{client_id = {}}}) end) end)",
            self.client_id
        );
        let nvim = active.rpc.client.nvim().clone();
        active.setup = Some(spawn(async move {
            let _ = nvim
                .request::<api::NvimExecLua, Object>((src.as_str(), ()))
                .await;
        }));
        Ok(())
    }

    async fn open_local(&mut self, alias: Option<&str>) -> Result<(Transport, Policies), Error> {
        use crate::HostInfo;
        use crate::socket;

        use tokio::process::Command;
        use tokio::time::sleep;

        use std::process::Stdio;

        let host = match alias {
            Some(alias) => self
                .config
                .hosts
                .get(alias)
                .ok_or_else(|| Error::Message(format!("Unknown host {alias}")))?,
            None => &self.config.default_spec,
        };
        let needs_info =
            host.nvim.needs_info() || host.socket.as_ref().is_some_and(|s| s.needs_info());
        let info = if needs_info {
            Some(HostInfo::local(None)?)
        } else {
            None
        };
        let nvim = host.nvim.resolve(info.as_ref(), &self.config.lua)?;
        let path = if let Some(s) = &host.socket {
            PathBuf::from(s.resolve(info.as_ref(), &self.config.lua)?)
        } else {
            socket::local_path(info.as_ref(), alias).await?
        };
        socket::check_local_path_length(&path, alias)?;

        let policies = host.policies.clone();
        if let Some(stream) = timeout(CHECK_TIMEOUT, check_local_socket(&path))
            .await
            .map_err(|_| Error::Timeout("Socket staleness check"))??
        {
            return Ok((Transport::local(stream), policies));
        }
        if host.socket.is_some()
            && let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).await?;
        }

        let mut cmd = Command::new(&nvim.prog);
        cmd.args(&nvim.args)
            .args(["--headless", "--listen"])
            .arg(&path)
            .envs(&nvim.env)
            .env("NAUVIM_CONNECTION", "local")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false);
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Message(format!("Start Neovim {}: {e}", nvim.prog)))?;
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(Error::Message(format!(
                    "Neovim exited during startup ({status})"
                )));
            }
            match UnixStream::connect(&path).await {
                Ok(stream) => {
                    // Dropping this non-killing handle delegates child reaping to
                    // Tokio; the persistent host remains independent of the UI.
                    drop(child);
                    return Ok((Transport::local(stream), policies));
                }
                Err(e)
                    if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {}
                Err(e) => {
                    return Err(e.into());
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(feature = "ssh")]
type RemoteMarker = Remote;
#[cfg(not(feature = "ssh"))]
type RemoteMarker = ();

async fn check_local_socket(path: &Path) -> Result<Option<UnixStream>, Error> {
    use std::os::unix::fs::FileTypeExt as _;
    match fs::symlink_metadata(path).await {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(m) if !m.file_type().is_socket() => {
            return Err(Error::Message(format!(
                "RPC endpoint is not a socket: {}",
                path.display()
            )));
        }
        Ok(_) => {}
    }
    match UnixStream::connect(path).await {
        Ok(s) => Ok(Some(s)),
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
            match fs::remove_file(path).await {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}
