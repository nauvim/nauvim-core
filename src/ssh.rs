use crate::config::program::Program;
use crate::config::ssh::{Preferences, Resolved, resolve as resolve_host};
use crate::resources::Remote;
use crate::{Config, Error, HostInfo, ProjectDirs, Transport};

use openrussh_client::config::{Chain, Host};
use openrussh_client::connect::{Connection, Context, Disconnect, MakeHandler};
use openrussh_client::known_hosts::{KnownHosts, KnownHostsHandler};
use openrussh_client::process::Command;
use openrussh_client::russh::ChannelStream;
use openrussh_client::russh::client::{Config as SshConfig, Msg};

use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;

use tokio::time::{Duration, sleep, timeout};

use std::borrow::Cow;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Handlers<'a> {
    known: KnownHosts,
    chain: &'a Chain,
    preferences: Vec<Preferences>,
}
impl MakeHandler for Handlers<'_> {
    type Handler = KnownHostsHandler;
    type Error = Infallible;
    fn make_handler(&self, host: &Host) -> Result<Context<Self::Handler>, Self::Error> {
        use std::{iter, ptr};

        let handler = self.known.handler(&host.dest.name, host.dest.port);
        let mut conf = SshConfig::default();
        if let Some(p) = iter::once(&self.chain.target)
            .chain(self.chain.bastions.as_deref().unwrap_or_default())
            .zip(&self.preferences)
            .find_map(|(candidate, preferences)| ptr::eq(candidate, host).then_some(preferences))
        {
            conf.preferred = p.preferred.clone();
            if !p.host_keys_explicit {
                handler.update_preferred_config(&mut conf.preferred);
            }
        } else {
            handler.update_preferred_config(&mut conf.preferred);
        }
        Ok(Context::new(handler, Arc::new(conf)))
    }
}

pub(crate) async fn open(
    config: &Config,
    alias: &str,
    reuse_only: bool,
) -> Result<(Transport, Remote), Error> {
    let Resolved { chain, preferences } = resolve_host(config, alias)?;
    let known = KnownHosts::parse_path(&config.known_hosts)?;
    let conn = Arc::new(
        Connection::connect(
            &chain,
            Handlers {
                known,
                chain: &chain,
                preferences,
            },
        )
        .await
        .map_err(Error::from)?,
    );
    let result = open_on(config, alias, &conn, reuse_only).await;
    match result {
        Ok(transport) => Ok((transport, conn)),
        Err(e) => {
            let _ = conn
                .disconnect(Disconnect::ByApplication, "Attachment failed", "en")
                .await;
            Err(e)
        }
    }
}

async fn open_on(
    config: &Config,
    alias: &str,
    conn: &Remote,
    reuse_only: bool,
) -> Result<Transport, Error> {
    use crate::socket::{self, DIR_PREFIX, Destination};

    let host = config.hosts.get(alias);
    let configured_socket = host.and_then(|h| h.socket.as_ref());
    let needs_info = host.is_some_and(|h| h.nvim.needs_info())
        || configured_socket.is_some_and(|s| s.needs_info());
    let info = if needs_info {
        Some(host_info(conn).await?)
    } else {
        None
    };
    let nvim = match host {
        Some(h) => h.nvim.resolve(info.as_ref(), &config.lua)?,
        None => Cow::Owned(Program::default()),
    };
    let socket = if let Some(s) = configured_socket {
        s.resolve(info.as_ref(), &config.lua)?
    } else {
        let name = socket::file_name(config.client.as_ref(), Destination::Ssh(alias))?;
        let uid = prepare_socket_dir(conn).await?;
        format!("{DIR_PREFIX}{uid}/{name}")
    };
    let stream = timeout(Duration::from_secs(5), check_socket(conn, &socket, &nvim))
        .await
        .map_err(|_| Error::Timeout("Remote socket staleness check"))??;
    if let Some(stream) = stream {
        return Ok(Transport::remote(stream));
    }
    if reuse_only {
        return Err(Error::Message(format!(
            "Reattach to Neovim on {alias:?} at {socket}: no running session"
        )));
    }
    // A detached shell job has no inherited channel I/O and survives SSH closure.
    // Values are positional shell arguments, never interpolated into the script.
    let script = "if [ -n \"$1\" ]; then mkdir -p \"$(dirname \"$1\")\" || exit; fi; shift; nohup \"$@\" </dev/null >/dev/null 2>/dev/null &";
    let create_parent = configured_socket.map_or("", |_| socket.as_str());
    let cmd = Command::new()
        .prog("sh")
        .arg("-c")
        .arg(script)
        .arg("nauvim")
        .arg(create_parent)
        .arg("env")
        .arg("--")
        .args(
            nvim.env
                .iter()
                .filter(|(name, _)| name.as_str() != "NAUVIM_CONNECTION")
                .map(|(name, value)| (name.as_str(), "=", value.as_str())),
        )
        .arg("NAUVIM_CONNECTION=ssh")
        .arg(nvim.prog.as_str())
        .args(nvim.args.iter().map(String::as_str))
        .args(["--headless", "--listen", socket.as_str()]);
    let output = conn
        .exec(cmd.complete(false))
        .await
        .map_err(Error::from)?
        .wait_with_output()
        .await;
    if !output.status.success() {
        return Err(Error::Message(format!(
            "Start remote Neovim: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    loop {
        match conn.handle().channel_open_direct_streamlocal(&socket).await {
            Ok(channel) => return Ok(Transport::remote(channel.into_stream())),
            Err(e) if conn.handle().is_closed() => return Err(e.into()),
            Err(_) => sleep(Duration::from_millis(20)).await,
        }
    }
}

async fn prepare_socket_dir(conn: &Remote) -> Result<u32, Error> {
    use crate::socket::DIR_PREFIX;

    let script = r#"
uid=$(id -u) || exit
case $uid in ''|*[!0-9]*) exit 1;; esac
dir=$1$uid
if ! mkdir -m 700 "$dir" 2>/dev/null; then
    user=$(id -un) || exit
    if [ "$(find "$dir" -prune -type d -user "$user" -perm 0700 -print)" != "$dir" ]; then
        printf '%s must be a directory owned by the effective user with mode 0700\n' "$dir" >&2
        exit 1
    fi
fi
printf '%s' "$uid"
"#;
    let output = conn
        .exec(
            Command::new()
                .prog("sh")
                .args(["-c", script, "nauvim", DIR_PREFIX])
                .complete(false),
        )
        .await
        .map_err(Error::from)?
        .wait_with_output()
        .await;
    if !output.status.success() {
        return Err(Error::Message(format!(
            "Prepare remote Neovim socket directory: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }
    let uid = String::from_utf8(output.stdout)?;
    uid.parse()
        .map_err(|e| Error::Message(format!("Read remote Neovim socket directory owner: {e}")))
}

async fn sftp(conn: &Remote) -> Result<SftpSession, Error> {
    let channel = conn
        .handle()
        .channel_open_session()
        .await
        .map_err(Error::from)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(Error::from)?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(Error::from)
}

async fn host_info(conn: &Remote) -> Result<HostInfo, Error> {
    let script = "printf '%s\\0' \"$(hostname)\" \"$(id -un)\" \"$HOME\" \"${XDG_CACHE_HOME:-$HOME/.cache}/nauvim\" \"${XDG_DATA_HOME:-$HOME/.local/share}/nauvim\" \"${XDG_RUNTIME_DIR:-/tmp/nauvim-runtime-$(id -u)}/nauvim\"";
    let output = conn
        .exec(
            Command::new()
                .prog("sh")
                .arg("-c")
                .arg(script)
                .complete(false),
        )
        .await
        .map_err(Error::from)?
        .wait_with_output()
        .await;
    if !output.status.success() {
        return Err(Error::Message("Acquire remote host information".into()));
    }
    let mut fields = output.stdout.split(|&b| b == 0);
    fn field<'a>(fields: &mut impl Iterator<Item = &'a [u8]>) -> Result<String, Error> {
        let value = fields
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Message("Incomplete remote HostInfo".into()))?;
        String::from_utf8(value.to_vec()).map_err(Error::from)
    }
    let host_name = field(&mut fields)?;
    let user = field(&mut fields)?;
    let home = field(&mut fields)?;
    let cache = field(&mut fields)?;
    let data = field(&mut fields)?;
    let runtime = field(&mut fields)?;
    let sftp = sftp(conn).await?;
    let ret = HostInfo {
        host_name,
        user,
        config_file: None,
        home_dir: resolve(&sftp, &home).await?,
        project_dirs: ProjectDirs {
            cache_dir: resolve(&sftp, &cache).await?,
            data_dir: resolve(&sftp, &data).await?,
            runtime_dir: resolve(&sftp, &runtime).await?,
        },
    };
    sftp.close().await.map_err(Error::from)?;
    Ok(ret)
}

async fn resolve(sftp: &SftpSession, path: &str) -> Result<PathBuf, Error> {
    match sftp.canonicalize(path).await {
        Ok(resolved) if Path::new(&resolved).is_absolute() => return Ok(PathBuf::from(resolved)),
        Ok(_) => {
            return Err(Error::Message(
                "Remote HostInfo path is not absolute".into(),
            ));
        }
        Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {}
        Err(e) => return Err(e.into()),
    }
    let mut resolved = if Path::new(path).is_absolute() {
        PathBuf::from("/")
    } else {
        PathBuf::from(sftp.canonicalize(".").await.map_err(Error::from)?)
    };
    if !resolved.is_absolute() {
        return Err(Error::Message(
            "Remote working directory is not absolute".into(),
        ));
    }
    let mut pending: VecDeque<_> = path.split('/').map(str::to_owned).collect();
    let mut links = 0_u8;
    while let Some(name) = pending.pop_front() {
        match name.as_str() {
            "" | "." => continue,
            ".." => {
                resolved.pop();
                continue;
            }
            _ => resolved.push(name),
        }
        match sftp
            .symlink_metadata(resolved.to_string_lossy().as_ref())
            .await
        {
            Ok(metadata) if metadata.is_symlink() => {
                if links == 40 {
                    return Err(Error::Message(
                        "Too many symbolic links in remote HostInfo path".into(),
                    ));
                }
                links = links.saturating_add(1);
                let target = sftp
                    .read_link(resolved.to_string_lossy().as_ref())
                    .await
                    .map_err(Error::from)?;
                resolved.pop();
                if Path::new(&target).is_absolute() {
                    resolved = PathBuf::from("/");
                }
                for component in target.split('/').rev() {
                    pending.push_front(component.to_owned());
                }
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(Error::Message(format!(
                    "Remote HostInfo path is not a directory: {}",
                    resolved.display()
                )));
            }
            Ok(_) => {}
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(resolved)
}

type Stream = ChannelStream<Msg>;
async fn check_socket(
    conn: &Remote,
    socket: &str,
    nvim: &Program,
) -> Result<Option<Stream>, Error> {
    let sftp = sftp(conn).await?;
    match sftp.symlink_metadata(socket).await {
        Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
            let _ = sftp.close().await;
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
        Ok(metadata)
            if !metadata
                .permissions
                .is_some_and(|mode| mode & 0o170000 == 0o140000) =>
        {
            let _ = sftp.close().await;
            return Err(Error::Message("Remote RPC endpoint is not a socket".into()));
        }
        Ok(_) => {}
    }
    match conn.handle().channel_open_direct_streamlocal(socket).await {
        Ok(channel) => {
            let _ = sftp.close().await;
            return Ok(Some(channel.into_stream()));
        }
        Err(_) => {
            // A forwarding failure is ambiguous. Only an explicit ECONNREFUSED
            // from the host-side Unix connect proves that this socket is stale.
            // Retain the configured program, arguments, and environment so a
            // shell or other launcher continues to forward the probe arguments.
            let script = "local p=assert(vim.uv.new_pipe(false)); p:connect(vim.fn.argv(0),vim.schedule_wrap(function(e) p:close(); if e and (e:match('^ECONNREFUSED') or e:match('^ENOENT')) then io.write('NAUVIM_STALE'); vim.cmd('qa!') else vim.cmd('cquit 2') end end))";
            let output = conn
                .exec(
                    Command::new()
                        .prog("env")
                        .arg("--")
                        .args(
                            nvim.env
                                .iter()
                                .map(|(name, value)| (name.as_str(), "=", value.as_str())),
                        )
                        .arg(nvim.prog.as_str())
                        .args(nvim.args.iter().map(String::as_str))
                        .args(["--headless", "-u", "NONE", "-i", "NONE", "-n", "-c"])
                        .arg(("lua ", script))
                        .arg("--")
                        .arg(socket)
                        .complete(false),
                )
                .await
                .map_err(Error::from)?
                .wait_with_output()
                .await;
            if !output.status.success() || output.stdout != b"NAUVIM_STALE" {
                return Err(Error::Message(
                    "Remote RPC socket check failed; endpoint retained".into(),
                ));
            }
            sftp.remove_file(socket).await.map_err(Error::from)?;
        }
    }
    sftp.close().await.map_err(Error::from)?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    use openrussh_client::config::{Auth, Dest, RequestTty};
    use openrussh_client::russh::{Preferred, kex};

    #[test]
    fn retains_distinct_algorithm_preferences_for_repeated_destinations() {
        let host = Host {
            dest: Dest {
                name: "same.example".into(),
                port: 22,
                user: "editor".into(),
            },
            auth: Auth::default(),
            tty: RequestTty::Never,
        };
        let chain = Chain {
            target: host.clone(),
            bastions: Some(vec![host].into_boxed_slice()),
        };
        let preferences = [kex::CURVE25519, kex::DH_G14_SHA256]
            .into_iter()
            .map(|kex| Preferences {
                preferred: Preferred {
                    kex: Cow::Owned(vec![kex]),
                    ..Preferred::default()
                },
                host_keys_explicit: true,
            })
            .collect();
        let handlers = Handlers {
            known: KnownHosts::parse(""),
            chain: &chain,
            preferences,
        };
        let (_, target) = handlers.make_handler(&chain.target).unwrap().into_inner();
        let hop = chain.bastions.as_ref().unwrap().first().unwrap();
        let (_, hop) = handlers.make_handler(hop).unwrap().into_inner();
        assert_eq!(target.preferred.kex.as_ref(), [kex::CURVE25519]);
        assert_eq!(hop.preferred.kex.as_ref(), [kex::DH_G14_SHA256]);
    }
}
