#![cfg(all(feature = "plugin-clipboard", target_os = "linux"))]

use nauvim_core::{Client, Config, Engine, EngineEvent};

use nvim_rpc_plugin::api;

use tempfile::tempdir;

use tokio::process::Command;
use tokio::time::{Duration, sleep, timeout};

use std::env;
use std::fs;
use std::path::Path;
use std::process::{self, Stdio};

fn lua_string(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\0', "\\000")
    )
}

struct Session {
    engine: Engine,
    host: Option<i64>,
}

impl Session {
    async fn open(root: &Path) -> Self {
        let nvim = env::var("NVIM_BIN").expect("Set NVIM_BIN to Neovim v0.12.6");
        let startup = "lua package.preload['nauvim-plugin.client']=function() return {setup=function(opts) _G.nauvim_clipboard_setup=opts.client_id end} end";
        let src = format!(
            r#"return {{clipboard="xclip",hosts={{local_host={{
            nvim={{prog={},args={{"-u","NONE","-i","NONE","-n","--cmd",{}}},env={{NVIM_LOG_FILE={}}}}},socket={},
        }}}}}}"#,
            lua_string(&nvim),
            lua_string(startup),
            lua_string(root.join("nvim.log").to_str().unwrap()),
            lua_string(root.join("clipboard host.sock").to_str().unwrap())
        );
        let file = root.join("config.lua");
        fs::write(&file, src).unwrap();
        let (config, ()) =
            Config::load(Some(&file), "org.example.nauvim.clipboard-test", |_| Ok(())).unwrap();
        let mut engine = Engine::new(config).unwrap();
        timeout(
            Duration::from_secs(20),
            engine.connect("local_host", 60, 16),
        )
        .await
        .unwrap()
        .unwrap();
        let host = engine
            .client()
            .unwrap()
            .nvim()
            .request::<api::NvimEval, i64>(("getpid()",))
            .await
            .unwrap();
        Self {
            engine,
            host: Some(host),
        }
    }

    fn client(&self) -> Client {
        self.engine.client().unwrap()
    }

    async fn lua(&self, source: &str) -> bool {
        timeout(
            Duration::from_secs(5),
            self.client()
                .nvim()
                .request::<api::NvimExecLua, bool>((source, ())),
        )
        .await
        .expect("Clipboard test Lua RPC timed out")
        .unwrap()
    }

    async fn notify(&self, arguments: &str) {
        let source = format!(
            r#"
            local chan = vim.api.nvim_list_uis()[1].chan
            vim.rpcnotify(chan, "nauvim-plugin/clipboard.copy", {arguments})
            vim.rpcrequest(chan, "nauvim-plugin/test.barrier", vim.NIL)
            return true
            "#
        );
        // The ignored-ID request confirms receipt and validation on this RPC
        // connection. Clipboard backend work can still be pending afterward.
        assert!(self.lua(&source).await);
    }

    async fn copy(&self, register: &str, text: &str) {
        let writes = env::var_os("NAUVIM_CLIPBOARD_TEST_DIR").map(|root| {
            let path = Path::new(&root).join("writes");
            let count = fs::read(&path).unwrap().len();
            (path, count)
        });
        self.notify(&format!(
            "{}, {}, {}",
            self.engine.client_id(),
            lua_string(register),
            lua_string(text)
        ))
        .await;
        let selection = if register == "*" {
            "primary"
        } else {
            "clipboard"
        };
        timeout(Duration::from_secs(5), async {
            loop {
                let written = writes
                    .as_ref()
                    .is_none_or(|(path, count)| fs::read(path).unwrap().len() > *count);
                if written && raw(selection).await == text.as_bytes() {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("Copy notification did not update the selected clipboard");
    }

    async fn error(&mut self) -> String {
        timeout(Duration::from_secs(5), async {
            loop {
                match self.engine.next_event().await {
                    EngineEvent::Error(error) => return error,
                    EngineEvent::Frame(_) => {}
                    _ => panic!("Unexpected lifecycle event while waiting for clipboard error"),
                }
            }
        })
        .await
        .expect("Clipboard operation did not report a client error")
    }

    async fn close(mut self) {
        let _ = timeout(Duration::from_secs(5), self.client().command("qa!")).await;
        self.engine.shutdown().await.unwrap();
        self.host = None;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(pid) = self.host.take() {
            let _ = process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn raw(selection: &str) -> Vec<u8> {
    let output = timeout(
        Duration::from_secs(5),
        Command::new("xclip")
            .args(["-selection", selection, "-out"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("Read clipboard exceeded five seconds")
    .unwrap();
    if output.status.success() {
        output.stdout
    } else {
        Vec::new()
    }
}

#[tokio::test]
#[ignore = "Requires NVIM_BIN v0.12.6 and local sockets; uses an isolated fake clipboard backend"]
async fn isolated_notifications_validate_text_and_gui_paste_uses_native_input() {
    use nauvim_core::Error;

    use nvim_rpc_plugin::{Error as RpcError, Object};
    use nvim_rpc_ui::ClientError;

    use std::iter::once;
    use std::os::unix::fs::PermissionsExt as _;

    const ROOT: &str = "NAUVIM_CLIPBOARD_TEST_DIR";
    if let Some(root) = env::var_os(ROOT) {
        let root = Path::new(&root);
        let mut session = Session::open(root).await;

        for (register, selection) in [("+", "clipboard"), ("*", "primary")] {
            for text in [
                "",
                "日本語🙂",
                "first\nliteral<Esc>\n",
                "two\n\nlines",
                "before\u{feff}after",
                "trailing\u{feff}",
            ] {
                session.copy(register, text).await;
                assert_eq!(fs::read(root.join(selection)).unwrap(), text.as_bytes());
            }
        }

        let previous = fs::read(root.join("clipboard")).unwrap();
        let writes = fs::read(root.join("writes")).unwrap();
        let id = session.engine.client_id();
        for arguments in [
            format!("{id}"),
            format!("{id}, '-', 'invalid register'"),
            format!("{id}, 1, 'invalid register type'"),
            format!("{id}, '+', {{'old lines shape'}}"),
            format!("{id}, '+', 'extra argument', false"),
            format!("{id}, '+', 'before'..string.char(0)..'after'"),
            format!("{id}, '+', string.char(239,187,191)..'after'"),
            format!("{id}, '+', string.char(255)"),
        ] {
            session.notify(&arguments).await;
            let error = session.error().await;
            assert!(!error.contains("system clipboard"), "{arguments}: {error}");
            assert_eq!(fs::read(root.join("writes")).unwrap(), writes);
            assert_eq!(fs::read(root.join("clipboard")).unwrap(), previous);
        }

        for bad_id in [
            "nil".to_owned(),
            "false".to_owned(),
            "0.5".to_owned(),
            "4294967296".to_owned(),
            session.engine.client_id().get().wrapping_add(1).to_string(),
        ] {
            session
                .notify(&format!("{bad_id}, false, string.char(0)"))
                .await;
        }
        assert_eq!(fs::read(root.join("writes")).unwrap(), writes);
        assert_eq!(fs::read(root.join("clipboard")).unwrap(), previous);
        assert!(
            session
                .lua(&format!(
                    r#"
                    local chan = vim.api.nvim_list_uis()[1].chan
                    local copy_ok = pcall(vim.rpcrequest, chan,
                        "nauvim-plugin/clipboard.copy", {id}, "+", "obsolete request")
                    local paste_ok = pcall(vim.rpcrequest, chan,
                        "nauvim-plugin/clipboard.paste", {id}, "+")
                    return not copy_ok and not paste_ok
                    "#
                ))
                .await
        );
        assert_eq!(fs::read(root.join("writes")).unwrap(), writes);

        let text = "\u{feff}日本語🙂\nliteral<Esc>\n終端\n";
        fs::write(root.join("clipboard"), text).unwrap();
        assert!(
            session
                .lua(
                    r#"
                    local function forbidden() error("provider paste callback was called") end
                    vim.g.clipboard = {
                        name = "clipboard-test",
                        copy = {["+"] = function() end, ["*"] = function() end},
                        paste = {["+"] = forbidden, ["*"] = forbidden},
                        cache_enabled = 0,
                    }
                    vim.api.nvim_buf_set_lines(0, 0, -1, false, {""})
                    vim.api.nvim_win_set_cursor(0, {1, 0})
                    local paste = vim.paste
                    vim.paste = function(lines, phase)
                        _G.nauvim_paste_phase = phase
                        return paste(lines, phase)
                    end
                    return true
                    "#,
                )
                .await
        );
        session.client().input("i").await.unwrap();
        timeout(Duration::from_secs(5), async {
            while !session.lua("return vim.fn.mode() == 'i'").await {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        assert!(session.engine.paste().await.unwrap());
        let lines = session
            .client()
            .nvim()
            .request::<api::NvimExecLua, Vec<String>>((
                "return vim.api.nvim_buf_get_lines(0,0,-1,false)",
                (),
            ))
            .await
            .unwrap();
        assert_eq!(lines, ["\u{feff}日本語🙂", "literal<Esc>", "終端", ""]);
        assert!(session.lua("return _G.nauvim_paste_phase == -1").await);

        assert!(
            session
                .lua("vim.paste=function() return false end; return true")
                .await
        );
        assert!(!session.engine.paste().await.unwrap());
        assert!(
            session
                .lua("vim.paste=function() error('fixture native paste failure') end; return true")
                .await
        );
        let error = session.engine.paste().await.unwrap_err();
        let displayed = error.to_string();
        let Error::Input(ClientError::Rpc(RpcError::Remote(Object::Array(mut fields)))) = error
        else {
            panic!("Expected the host's native paste error");
        };
        let _kind = fields.next().unwrap();
        let Object::Str(message) = fields.next().unwrap() else {
            panic!("Expected native paste error text");
        };
        assert!(
            message
                .as_str()
                .unwrap()
                .contains("fixture native paste failure")
        );
        assert_eq!(session.error().await, displayed);

        fs::write(root.join("read-fail"), "").unwrap();
        let error = session.engine.paste().await.unwrap_err().to_string();
        assert!(error.contains("fixture clipboard read failure"), "{error}");
        assert_eq!(session.error().await, error);
        assert!(session.lua("return true").await);

        let owners: Vec<_> = ["clipboard.pid", "primary.pid"]
            .into_iter()
            .map(|file| fs::read_to_string(root.join(file)).unwrap())
            .collect();
        session.close().await;
        for pid in owners {
            assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists());
        }
        return;
    }

    env::var_os("NVIM_BIN").expect("Set NVIM_BIN to Neovim v0.12.6");
    let root = tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let program = bin.join("xclip");
    fs::write(
        &program,
        r#"#!/bin/sh
selection=clipboard
mode=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -selection) shift; selection=$1 ;;
        -in|-out) mode=$1 ;;
    esac
    shift
done
case "$mode" in
    -in)
        /bin/cat > "$NAUVIM_CLIPBOARD_TEST_DIR/$selection"
        printf x >> "$NAUVIM_CLIPBOARD_TEST_DIR/writes"
        printf '%s\n' "$$" > "$NAUVIM_CLIPBOARD_TEST_DIR/$selection.pid"
        exec /bin/sleep 60
        ;;
    -out)
        if [ -e "$NAUVIM_CLIPBOARD_TEST_DIR/read-fail" ]; then
            printf 'fixture clipboard read failure' >&2
            exit 1
        fi
        exec /bin/cat "$NAUVIM_CLIPBOARD_TEST_DIR/$selection"
        ;;
    *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    for file in ["clipboard", "primary", "writes"] {
        fs::write(root.path().join(file), "").unwrap();
    }
    let path =
        env::join_paths(once(bin).chain(env::split_paths(&env::var_os("PATH").unwrap()))).unwrap();
    let status = timeout(
        Duration::from_secs(30),
        Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "isolated_notifications_validate_text_and_gui_paste_uses_native_input",
                "--ignored",
                "--nocapture",
            ])
            .env(ROOT, root.path())
            .env("PATH", path)
            .env("DISPLAY", "clipboard-fixture.test:0")
            .env("APPIMAGE_EXTRACT_AND_RUN", "1")
            .kill_on_drop(true)
            .status(),
    )
    .await
    .expect("Isolated clipboard fixture exceeded thirty seconds")
    .unwrap();
    assert!(
        status.success(),
        "Isolated clipboard fixture failed: {status}"
    );
}

#[tokio::test]
#[ignore = "Requires NVIM_BIN v0.12.6, xclip, DISPLAY and local sockets; preserves existing clipboard bytes"]
async fn live_copy_notifications_and_gui_paste_use_the_desktop_clipboard() {
    struct Restore(Vec<(&'static str, Vec<u8>)>);
    impl Restore {
        fn restore(&mut self) {
            use std::io::Write as _;

            for (selection, bytes) in &self.0 {
                let Ok(mut child) = process::Command::new("xclip")
                    .args(["-selection", selection, "-in"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    continue;
                };
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(bytes);
                }
                let _ = child.wait();
            }
        }
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            self.restore();
        }
    }
    let mut restore = Restore(vec![
        ("clipboard", raw("clipboard").await),
        ("primary", raw("primary").await),
    ]);
    let root = tempdir().unwrap();
    let session = Session::open(root.path()).await;

    for (register, selection) in [("+", "clipboard"), ("*", "primary")] {
        for text in ["日本語🙂\n終端\n", "literal<Esc>\n", "ab\n日本"] {
            session.copy(register, text).await;
            assert_eq!(raw(selection).await, text.as_bytes());
        }
    }
    session.client().input("i").await.unwrap();
    timeout(Duration::from_secs(5), async {
        while !session.lua("return vim.fn.mode() == 'i'").await {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(session.engine.paste().await.unwrap());
    let lines = session
        .client()
        .nvim()
        .request::<api::NvimExecLua, Vec<String>>((
            "return vim.api.nvim_buf_get_lines(0,0,-1,false)",
            (),
        ))
        .await
        .unwrap();
    assert_eq!(lines, ["ab", "日本"]);
    let mut owners = Vec::new();
    for task in fs::read_dir("/proc/self/task").unwrap() {
        let children = fs::read_to_string(task.unwrap().path().join("children")).unwrap();
        for pid in children.split_whitespace() {
            if fs::read(format!("/proc/{pid}/comm")).ok().as_deref() == Some(b"xclip\n") {
                owners.push(pid.to_owned());
            }
        }
    }
    owners.sort();
    owners.dedup();
    assert!(
        !owners.is_empty(),
        "Clipboard ownership was not established"
    );
    session.close().await;
    for pid in owners {
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    restore.restore();
    for (selection, original) in &restore.0 {
        assert_eq!(raw(selection).await, *original);
    }
    restore.0.clear();
}

#[tokio::test]
async fn unavailable_backend_reports_client_error_without_preventing_engine_creation() {
    let root = tempdir().unwrap();
    let file = root.path().join("init.lua");
    fs::write(&file, "return {clipboard='not-a-clipboard-backend'}").unwrap();
    let (config, ()) =
        Config::load(Some(&file), "org.example.nauvim.clipboard-test", |_| Ok(())).unwrap();
    let mut engine = Engine::new(config).unwrap();
    let event = timeout(Duration::from_secs(1), engine.next_event())
        .await
        .unwrap();
    let EngineEvent::Error(error) = event else {
        panic!("Expected backend availability diagnostic")
    };
    assert_eq!(error, "Unknown clipboard backend: not-a-clipboard-backend");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_timeout_reports_client_error_and_uses_default() {
    let root = tempdir().unwrap();
    let file = root.path().join("init.lua");
    fs::write(
        &file,
        "return {clipboard={backend='not-a-clipboard-backend',timeout_ms=3.0}}",
    )
    .unwrap();
    let (config, ()) =
        Config::load(Some(&file), "org.example.nauvim.clipboard-test", |_| Ok(())).unwrap();
    assert_eq!(config.clipboard().timeout(), Duration::from_secs(3));
    let mut engine = Engine::new(config).unwrap();
    let event = timeout(Duration::from_secs(1), engine.next_event())
        .await
        .unwrap();
    let EngineEvent::Error(error) = event else {
        panic!("Expected invalid timeout diagnostic")
    };
    assert_eq!(
        error,
        "clipboard.timeout_ms must have integer type and be at least 1; using 3000 ms"
    );
    let event = timeout(Duration::from_secs(1), engine.next_event())
        .await
        .unwrap();
    let EngineEvent::Error(error) = event else {
        panic!("Expected backend diagnostic after timeout fallback")
    };
    assert_eq!(error, "Unknown clipboard backend: not-a-clipboard-backend");
    engine.shutdown().await.unwrap();
}
