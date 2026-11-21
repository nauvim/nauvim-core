#![cfg(all(feature = "ssh", unix))]

use nauvim_core::{Client, ClientId, Config, Engine, EngineEvent};

use nvim_rpc_plugin::api;

use tempfile::TempDir;

use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::time::{Duration, sleep, timeout};

use std::env;
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};

fn lua_string(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

fn shell_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn sshd_bin() -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let configured = env::var_os("SSHD_BIN");
    let name = configured
        .as_deref()
        .map(Path::new)
        .unwrap_or(Path::new("sshd"));
    let is_command = name.file_name() == Some(name.as_os_str());
    let path = env::var_os("PATH").filter(|_| is_command);
    let fallback = match configured {
        None => Some(PathBuf::from("/usr/sbin/sshd")),
        Some(_) if !is_command => Some(name.to_path_buf()),
        Some(_) => None,
    };
    let bin = path
        .iter()
        .flat_map(env::split_paths)
        .map(|dir| dir.join(name))
        .chain(fallback)
        .find(|path| {
            fs::metadata(path)
                .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        })
        .unwrap_or_else(|| {
            panic!(
                "Cannot find executable {}. Install the OpenSSH server or set SSHD_BIN to its sshd executable.",
                name.display()
            )
        });
    // OpenSSH can re-exec itself and needs an absolute executable path.
    std::path::absolute(&bin)
        .unwrap_or_else(|err| panic!("Resolve sshd path {}: {err}", bin.display()))
}

struct Server {
    root: TempDir,
    daemon: Child,
    socket: PathBuf,
    nvim: PathBuf,
    port: u16,
    user: String,
    host_pid: Option<i64>,
}

impl Server {
    async fn start() -> Self {
        use tempfile::tempdir;

        use tokio::process::Command as AsyncCommand;

        use std::net::TcpListener;
        use std::os::unix::fs::PermissionsExt as _;

        let bin = env::var("NVIM_BIN").expect("Set NVIM_BIN to Neovim v0.12.6");
        let sshd = sshd_bin();
        let root = tempdir().unwrap();
        let user = process::Command::new("id").arg("-un").output().unwrap();
        assert!(user.status.success());
        let user = String::from_utf8(user.stdout).unwrap().trim().to_owned();
        for name in ["identity", "host-key"] {
            let output = process::Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.path().join(name))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let config = format!(
            r#"
Port {port}
ListenAddress 127.0.0.1
HostKey "{}"
PidFile "{}"
AuthorizedKeysFile "{}"
StrictModes no
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin yes
AllowUsers {user}
AllowTcpForwarding yes
AllowStreamLocalForwarding yes
PermitTTY no
Subsystem sftp internal-sftp
LogLevel DEBUG2
"#,
            root.path().join("host-key").display(),
            root.path().join("sshd.pid").display(),
            root.path().join("identity.pub").display()
        );
        fs::write(root.path().join("sshd_config"), config).unwrap();
        let host_key = fs::read_to_string(root.path().join("host-key.pub")).unwrap();
        fs::write(
            root.path().join("known_hosts"),
            format!("[127.0.0.1]:{port} {host_key}"),
        )
        .unwrap();
        fs::write(
            root.path().join("ssh_config"),
            "Host 127.0.0.1\n User must-be-overridden\n Port 1\n RequestTTY force\n",
        )
        .unwrap();
        let nvim = root.path().join("spaced nvim");
        fs::write(
            &nvim,
            format!(
                "#!/bin/sh\nif [ -t 0 ] || [ -t 1 ]; then exit 97; fi\nexport APPIMAGE_EXTRACT_AND_RUN=1\nexec {} \"$@\"\n",
                shell_string(&bin)
            ),
        )
        .unwrap();
        fs::set_permissions(&nvim, fs::Permissions::from_mode(0o700)).unwrap();
        let log = fs::File::create(root.path().join("sshd.log")).unwrap();
        let daemon = AsyncCommand::new(&sshd)
            .args(["-D", "-e", "-f"])
            .arg(root.path().join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|err| panic!("Failed to launch sshd at {}: {err}", sshd.display()));
        let mut server = Self {
            socket: root.path().join("remote host.sock"),
            nvim,
            root,
            daemon,
            port,
            user,
            host_pid: None,
        };
        timeout(Duration::from_secs(5), async {
            loop {
                if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                    break;
                }
                assert!(
                    server.daemon.try_wait().unwrap().is_none(),
                    "sshd failed:\n{}",
                    server.log()
                );
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("sshd did not start");
        server
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.path().join("sshd.log")).unwrap_or_default()
    }

    fn config(&self, socket: &Path) -> Config {
        let startup = r#"lua package.preload['nauvim-plugin.client']=function() return {setup=function(opts) _G.nauvim_ssh_setup=opts.client_id end} end; _G.nauvim_request=function(method,...) return vim.rpcrequest(vim.api.nvim_list_uis()[1].chan,'nauvim-plugin/'..method,_G.nauvim_ssh_setup,...) end; _G.nauvim_notify=function(method,...) vim.rpcnotify(vim.api.nvim_list_uis()[1].chan,'nauvim-plugin/'..method,_G.nauvim_ssh_setup,...) end"#;
        let src = format!(
            r#"
local client = require("nauvim-client-info")
local function check_host(info)
    assert(info.config_file == nil)
    assert(info.user == client.user)
    assert(info.host_name == client.host_name)
    assert(info.home_dir:sub(1,1) == "/")
    for _, path in pairs(info.project_dirs) do assert(path:sub(1,1) == "/") end
end
return {{
    hosts = {{ remote = {{
        nvim = function(info) check_host(info); return {{prog = "sh", args = {{"-c", {}, "nauvim", {}, "-u", "NONE", "-i", "NONE", "-n", "--cmd", {}}}, env = {{NAUVIM_LITERAL = {}, NAUVIM_ARGV_FILE = {}}}}} end,
        socket = function(info) check_host(info); return {} end,
        ssh = {{ host_name = "127.0.0.1", user = {}, port = {},
            identity_file = {}, identities_only = true,
            host_key_algorithms = {{"ssh-ed25519"}}, kex_algorithms = {{"curve25519-sha256"}},
        }},
    }} }},
    ssh = {{ config = {}, known_hosts = {} }},
}}
"#,
            lua_string(r#"printf '%s\0' "$@" > "$NAUVIM_ARGV_FILE"; exec "$@""#),
            lua_string(self.nvim.to_str().unwrap()),
            lua_string(startup),
            lua_string("literal $HOME; 'quoted' \"double\" 日本語"),
            lua_string(self.root.path().join("argv record").to_str().unwrap()),
            lua_string(socket.to_str().unwrap()),
            lua_string(&self.user),
            self.port,
            lua_string(self.root.path().join("identity").to_str().unwrap()),
            lua_string(self.root.path().join("ssh_config").to_str().unwrap()),
            lua_string(self.root.path().join("known_hosts").to_str().unwrap())
        );
        let file = self.root.path().join("config.lua");
        fs::write(&file, src).unwrap();
        let (config, ()) =
            Config::load(Some(&file), "org.example.nauvim.ssh-test", |_| Ok(())).unwrap();
        config
    }

    async fn stop(mut self) {
        self.kill_host();
        self.daemon.kill().await.unwrap();
        self.daemon.wait().await.unwrap();
    }

    fn kill_host(&mut self) {
        if let Some(pid) = self.host_pid.take() {
            let _ = process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        use std::thread;

        self.kill_host();
        let _ = self.daemon.start_kill();
        if thread::panicking() {
            eprintln!("sshd test log:\n{}", self.log());
        }
    }
}

async fn integer(client: &Client, src: &str) -> i64 {
    timeout(
        Duration::from_secs(10),
        client.nvim().request::<api::NvimExecLua, i64>((src, ())),
    )
    .await
    .expect("remote Lua request timed out")
    .unwrap()
}

async fn wait_setup(client: &Client, client_id: ClientId) {
    timeout(Duration::from_secs(5), async {
        loop {
            let ready = client
                .nvim()
                .request::<api::NvimExecLua, bool>((
                    format!("return _G.nauvim_ssh_setup == {client_id}").as_str(),
                    (),
                ))
                .await
                .unwrap();
            if ready {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("asynchronous remote plugin setup did not run");
}

async fn welcome(engine: &mut Engine) -> Option<String> {
    timeout(Duration::from_secs(5), async {
        loop {
            match engine.next_event().await {
                EngineEvent::Welcome { error } => return error,
                EngineEvent::Error(error) => panic!("Unexpected remote engine error: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("Remote connection did not return to the welcome page")
}

#[cfg(feature = "plugin-local-forward")]
async fn forwarding(client: &Client, root: &Path, base: &Path) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, UnixListener, UnixStream};
    use tokio::spawn;

    let host = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host_port = host.local_addr().unwrap().port();
    let task = spawn(async move {
        let (mut socket, _) = host.accept().await.unwrap();
        let mut input = [0; 4];
        socket.read_exact(&mut input).await.unwrap();
        assert_eq!(&input, b"ping");
        socket.write_all(b"pong").await.unwrap();
    });
    let handle = integer(
        client,
        &format!("return nauvim_request('local-forward.open_tcp',0,'127.0.0.1',{host_port})"),
    )
    .await;
    let port = integer(
        client,
        &format!("return nauvim_request('local-forward.get_port',{handle})"),
    )
    .await;
    let mut socket = TcpStream::connect(("127.0.0.1", u16::try_from(port).unwrap()))
        .await
        .unwrap();
    socket.write_all(b"ping").await.unwrap();
    let mut output = [0; 4];
    socket.read_exact(&mut output).await.unwrap();
    assert_eq!(&output, b"pong");
    task.await.unwrap();

    let host_socket = root.join("host echo.sock");
    let local_socket = base.join("client echo.sock");
    let host = UnixListener::bind(&host_socket).unwrap();
    let task = spawn(async move {
        let (mut socket, _) = host.accept().await.unwrap();
        let mut input = [0; 4];
        socket.read_exact(&mut input).await.unwrap();
        assert_eq!(&input, b"ping");
        socket.write_all(b"pong").await.unwrap();
    });
    let handle = integer(
        client,
        &format!(
            "return nauvim_request('local-forward.open_socket',{}, {})",
            lua_string("client echo.sock"),
            lua_string(host_socket.to_str().unwrap())
        ),
    )
    .await;
    let mut socket = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(socket) = UnixStream::connect(&local_socket).await {
                break socket;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    socket.write_all(b"ping").await.unwrap();
    socket.read_exact(&mut output).await.unwrap();
    assert_eq!(&output, b"pong");
    task.await.unwrap();
    integer(
        client,
        &format!("nauvim_notify('local-forward.clear',{handle}); return 0"),
    )
    .await;
}

#[cfg(feature = "plugin-filesystem")]
async fn copy(client: &Client, root: &Path, base: &Path) {
    use std::os::unix::fs::symlink;

    let source = root.join("host source");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("nested/日本語.txt"), b"SFTP contents").unwrap();
    symlink("nested/日本語.txt", source.join("relative-link")).unwrap();
    let _unsupported = UnixListener::bind(source.join("ignored.sock")).unwrap();
    let handle = integer(
        client,
        &format!(
            "return nauvim_request('filesystem.copy','/remote-copy',{},true)",
            lua_string(source.to_str().unwrap())
        ),
    )
    .await;
    timeout(Duration::from_secs(5), async {
        loop {
            if fs::read(base.join("remote-copy/nested/日本語.txt"))
                .ok()
                .as_deref()
                == Some(b"SFTP contents")
                && fs::read(base.join("remote-copy/relative-link"))
                    .ok()
                    .as_deref()
                    == Some(b"SFTP contents")
            {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("SFTP copy did not produce its files");
    assert!(!base.join("remote-copy/ignored.sock").exists());
    assert!(
        !fs::symlink_metadata(base.join("remote-copy/relative-link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    integer(
        client,
        &format!("nauvim_notify('filesystem.clear',{handle}); return 0"),
    )
    .await;
    timeout(Duration::from_secs(5), async {
        while base.join("remote-copy").exists() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("SFTP copy destination was not cleared");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Requires local sshd, ssh-keygen, NVIM_BIN v0.12.6, and loopback/Unix socket permissions"]
async fn remote_attachment_sftp_forwarding_and_persistent_reattachment() {
    use std::str;

    timeout(Duration::from_secs(60), async {
        let mut server = Server::start().await;
        drop(UnixListener::bind(&server.socket).unwrap());
        let mut engine = Engine::new(server.config(&server.socket)).unwrap();
        engine.connect("remote", 60, 16).await.unwrap();
        let client = engine.client().unwrap();
        let pid = integer(&client, "return vim.fn.getpid()").await;
        server.host_pid = Some(pid);
        wait_setup(&client, engine.client_id()).await;
        let env = client
            .nvim()
            .request::<api::NvimExecLua, String>(("return vim.env.NAUVIM_LITERAL", ()))
            .await
            .unwrap();
        assert_eq!(env, "literal $HOME; 'quoted' \"double\" 日本語");
        let recorded = fs::read(server.root.path().join("argv record")).unwrap();
        let argv: Vec<_> = recorded
            .split(|&b| b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| str::from_utf8(v).unwrap())
            .collect();
        assert!(argv.ends_with(&["--headless", "--listen", server.socket.to_str().unwrap()]));
        assert_eq!(argv.first().copied(), server.nvim.to_str());
        assert_eq!(
            argv.get(1..7),
            Some(&["-u", "NONE", "-i", "NONE", "-n", "--cmd"][..])
        );
        #[cfg(feature = "plugin-local-forward")]
        forwarding(&client, server.root.path(), engine.temp_dir()).await;
        #[cfg(feature = "plugin-filesystem")]
        copy(&client, server.root.path(), engine.temp_dir()).await;
        engine.detach().await.unwrap();
        engine.shutdown().await.unwrap();
        assert!(server.socket.exists());
        assert!(client.command("echo 'closed'").await.is_err());

        let known = server.root.path().join("known_hosts");
        let original = fs::read(&known).unwrap();
        let different_key = fs::read_to_string(server.root.path().join("identity.pub")).unwrap();
        fs::write(
            &known,
            format!("[127.0.0.1]:{} {different_key}", server.port),
        )
        .unwrap();
        let mut rejected = Engine::new(server.config(&server.socket)).unwrap();
        assert!(
            rejected.connect("remote", 60, 16).await.is_err(),
            "Mismatched known host key was accepted"
        );
        rejected.shutdown().await.unwrap();
        assert!(server.socket.exists());
        fs::write(&known, original).unwrap();

        let mut engine = Engine::new(server.config(&server.socket)).unwrap();
        engine.connect("remote", 60, 16).await.unwrap();
        let client = engine.client().unwrap();
        assert_eq!(integer(&client, "return vim.fn.getpid()").await, pid);
        wait_setup(&client, engine.client_id()).await;
        let _ = client.command("detach").await;
        assert_eq!(
            welcome(&mut engine).await,
            None,
            "Host :detach is a graceful UI departure"
        );
        assert!(engine.client().is_none());
        engine.connect("remote", 60, 16).await.unwrap();
        let client = engine.client().unwrap();
        assert_eq!(integer(&client, "return vim.fn.getpid()").await, pid);
        wait_setup(&client, engine.client_id()).await;
        let _ = client.command("qa!").await;
        assert_eq!(
            welcome(&mut engine).await,
            None,
            "Host :qa! is a graceful shutdown"
        );
        engine.shutdown().await.unwrap();
        server.host_pid = None;

        let occupied = server.root.path().join("ordinary-file");
        fs::write(&occupied, b"must survive").unwrap();
        let mut engine = Engine::new(server.config(&occupied)).unwrap();
        assert!(engine.connect("remote", 60, 16).await.is_err());
        assert_eq!(fs::read(&occupied).unwrap(), b"must survive");
        engine.shutdown().await.unwrap();
        assert!(
            !server.log().contains("request pty-req"),
            "Client requested a pseudo-terminal"
        );
        server.stop().await;
    })
    .await
    .expect("remote integration test timed out");
}
