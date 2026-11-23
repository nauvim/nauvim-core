use nauvim_core::{Client, Config, Engine, EngineEvent};

mod support;
use support::checkpoint::{
    assert_no_markers, hidden_grids, leave_inactive_tab, next_frame, restored_grids,
    revisit_inactive_tab,
};
use support::is_clipboard_detection_error;

use nvim_rpc_plugin::{Object, api};

use tempfile::tempdir;

use tokio::process::Command;
use tokio::time::{Duration, sleep, timeout};

use std::env;
use std::fs;
use std::path::Path;

fn lua_string(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

fn config(root: &Path, socket: &Path) -> Config {
    // Exercise core registration without requiring the separate Lua component.
    let startup = r#"lua package.preload['nauvim-plugin.client']=function() return {
        setup=function(opts) _G.nauvim_test_setup_id=opts.client_id end
    } end"#;
    session_config(root, socket, Some(startup))
}

fn session_config(root: &Path, socket: &Path, startup: Option<&str>) -> Config {
    let bin = env::var("NVIM_BIN").expect("set NVIM_BIN to Neovim v0.12.6");
    let extra_args = startup
        .map(|script| format!(", \"--cmd\", {}", lua_string(script)))
        .unwrap_or_default();
    let src = format!(
        r#"local program = {{prog = {}, args = {{"-u", "NONE", "-i", "NONE", "-n"{extra_args}}}}}
        return {{hosts = {{
            local_host = {{
                nvim = program,
                socket = {},
            }},
            next_host = {{nvim = program, socket = {}}},
        }}}}"#,
        lua_string(&bin),
        lua_string(socket.to_str().unwrap()),
        lua_string(socket.with_extension("next.sock").to_str().unwrap())
    );
    let path = root.join("config.lua");
    fs::write(&path, src).unwrap();
    let (config, ()) =
        Config::load(Some(&path), "org.example.nauvim.session-test", |_| Ok(())).unwrap();
    config
}

async fn lua_bool(client: &Client, src: &str) -> bool {
    timeout(
        Duration::from_secs(5),
        client.nvim().request::<api::NvimExecLua, bool>((src, ())),
    )
    .await
    .expect("Lua RPC timed out")
    .unwrap()
}

async fn pid(client: &Client) -> i64 {
    client
        .nvim()
        .request::<api::NvimEval, i64>(("getpid()",))
        .await
        .unwrap()
}

async fn wait_setup(engine: &Engine, client: &Client) {
    let src = format!("return _G.nauvim_test_setup_id == {}", engine.client_id());
    timeout(Duration::from_secs(5), async {
        while !lua_bool(client, &src).await {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("host-side asynchronous setup did not run");
}

async fn next_welcome(engine: &mut Engine) -> Option<String> {
    timeout(Duration::from_secs(5), async {
        loop {
            match engine.next_event().await {
                EngineEvent::Welcome { error } => return error,
                EngineEvent::Frame(_) => {}
                EngineEvent::Error(e) if is_clipboard_detection_error(&e) => {}
                EngineEvent::Error(e) => panic!("unexpected error event: {e}"),
                _ => panic!("unexpected plugin event while awaiting welcome page"),
            }
        }
    })
    .await
    .expect("host disconnection did not return to the welcome page")
}

async fn quit(engine: &mut Engine) {
    if let Some(client) = engine.client() {
        // Neovim may close the stream before replying to its own exit command.
        let _ = timeout(Duration::from_secs(5), client.command("qa!")).await;
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejects_zero_dimensions_before_startup() {
    let mut engine = Engine::new(Config::default()).unwrap();
    for (width, height) in [(0, 20), (80, 0)] {
        let err = engine.connect("local", width, height).await.err().unwrap();
        assert!(err.to_string().contains("dimensions"));
        assert!(engine.client().is_none());
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn reports_overlong_local_socket_paths_on_the_welcome_page() {
    let root = tempdir().unwrap();
    let file = root.path().join("config.lua");
    let absolute = format!("/{}", "é".repeat(60));
    let relative = format!("./{}", "界".repeat(40));
    let src = format!(
        r#"return {{hosts = {{
            ['local "one"'] = {{nvim = 'unused', socket = {}}},
            local_two = {{nvim = 'unused', socket = function(_) return {} end}},
        }}}}"#,
        lua_string(&absolute),
        lua_string(&relative),
    );
    fs::write(&file, src).unwrap();
    let (config, ()) =
        Config::load(Some(&file), "org.example.nauvim.session-test", |_| Ok(())).unwrap();
    let mut engine = Engine::new(config).unwrap();

    for (alias, path) in [(r#"local "one""#, &absolute), ("local_two", &relative)] {
        let error = engine.connect(alias, 60, 16).await.unwrap_err().to_string();
        assert!(
            error.contains(&format!("({} bytes, maximum ", path.len())),
            "{error}"
        );
        assert!(
            error.contains(&format!("Set hosts[{alias:?}].socket to a shorter path.")),
            "{error}"
        );
        assert!(error.ends_with(path), "{error}");
        let welcome = timeout(Duration::from_secs(5), async {
            loop {
                if let EngineEvent::Welcome { error } = engine.next_event().await {
                    break error;
                }
            }
        })
        .await
        .expect("connection failure did not return to the welcome page");
        assert_eq!(welcome, Some(error));
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_restores_a_checkpoint_at_startup_on_a_default_local_socket() {
    let nvim = env::var("NVIM_BIN").expect("set NVIM_BIN to Neovim v0.12.6");
    let root = tempdir().unwrap();
    let alias = root.path().file_name().unwrap().to_str().unwrap();
    let src = format!(
        r#"local alias = {}
        return {{default_host = alias, hosts = {{[alias] = {{nvim = {{prog = {}, args = {{'-u', 'NONE', '-i', 'NONE', '-n'}}}}}}}}}}"#,
        lua_string(alias),
        lua_string(&nvim),
    );
    let file = root.path().join("config.lua");
    fs::write(&file, src).unwrap();
    let config = || {
        Config::load(Some(&file), "org.example.nauvim.session-test", |_| Ok(()))
            .unwrap()
            .0
    };
    let mut first = Engine::new(config()).unwrap();
    first.connect(alias, 60, 16).await.unwrap();
    let client = first.client().unwrap();
    let process_id = pid(&client).await;
    let address = client
        .nvim()
        .request::<api::NvimEval, String>(("v:servername",))
        .await
        .unwrap();
    leave_inactive_tab(&client).await;
    let checkpoint = first.checkpoint().unwrap();
    first.shutdown().await.unwrap();

    let mut second = Engine::new(config()).unwrap();
    second
        .start_with_checkpoint(60, 16, checkpoint)
        .await
        .unwrap();
    let reattached_pid = pid(&second.client().unwrap()).await;
    restored_grids(&mut second).await;
    quit(&mut second).await;

    assert_eq!(reattached_pid, process_id);
    assert!(address.starts_with("/tmp/nauvim-sockets-"), "{address}");
    assert!(address.len() <= 103, "{address}");
}

#[tokio::test]
async fn sigterm_removes_process_temporary_directory_before_shutdown_event() {
    const MARKER: &str = "NAUVIM_SIGTERM_TEST_MARKER";
    if let Some(marker) = env::var_os(MARKER) {
        let file = Path::new(&marker).with_extension("lua");
        fs::write(&file, "return {hosts={choose={}}}").unwrap();
        let (config, ()) =
            Config::load(Some(&file), "org.example.nauvim.session-test", |_| Ok(())).unwrap();
        let mut engine = Engine::new(config).unwrap();
        engine.start(80, 24).await.unwrap();
        let temp = engine.temp_dir().to_path_buf();
        fs::write(marker, temp.as_os_str().as_encoded_bytes()).unwrap();
        loop {
            if matches!(engine.next_event().await, EngineEvent::Shutdown) {
                break;
            }
        }
        assert!(!temp.exists());
        engine.shutdown().await.unwrap();
        return;
    }

    let root = tempdir().unwrap();
    let marker = root.path().join("ready");
    let mut child = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "sigterm_removes_process_temporary_directory_before_shutdown_event",
            "--nocapture",
        ])
        .env(MARKER, &marker)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("SIGTERM fixture exited before readiness: {status}");
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let temp = fs::read_to_string(&marker).unwrap();
    assert!(Path::new(&temp).is_dir());
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!Path::new(&temp).exists());
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_input_registration_detachment_and_persistent_reattachment() {
    use nauvim_core::KeyInput;

    let root = tempdir().unwrap();
    let socket = root.path().join("persistent session.sock");
    let mut engine = Engine::new(config(root.path(), &socket)).unwrap();
    let client_id = engine.client_id();
    let process_temp = engine.temp_dir().to_path_buf();
    timeout(
        Duration::from_secs(20),
        engine.connect("local_host", 80, 24),
    )
    .await
    .unwrap()
    .unwrap();
    let client = engine.client().unwrap();
    let host_pid = pid(&client).await;
    wait_setup(&engine, &client).await;
    assert_eq!(engine.client_id(), client_id);
    assert!(
        lua_bool(
            &client,
            &format!(
                r#"
        local uis = vim.api.nvim_list_uis()
        assert(#uis == 1)
        local ui = uis[1]
        local info = vim.api.nvim_get_chan_info(ui.chan).client
        local version = info.version
        return info.name == "nauvim-client-{client_id}" and info.type == "ui"
            and version.major == {major} and version.minor == {minor} and version.patch == {patch}
            and vim.tbl_count(info.methods) == 0
            and vim.tbl_count(info.attributes) == 1
            and info.attributes.license == "MIT OR Apache-2.0"
            and ui.rgb and ui.ext_linegrid and ui.ext_multigrid and ui.ext_cmdline
            and ui.ext_messages and ui.ext_popupmenu and ui.ext_tabline
            and ui.ext_hlstate and ui.ext_termcolors
        "#,
                major = env!("CARGO_PKG_VERSION_MAJOR"),
                minor = env!("CARGO_PKG_VERSION_MINOR"),
                patch = env!("CARGO_PKG_VERSION_PATCH"),
            )
        )
        .await
    );
    assert!(!next_frame(&mut engine).await.state.grids.is_empty());

    client.input("i").await.unwrap();
    client
        .send_key(KeyInput::Text {
            text: "日本語<Esc>",
        })
        .await
        .unwrap();
    client.input("<Esc>").await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let line = client
                .nvim()
                .request::<api::NvimGetCurrentLine, String>(())
                .await
                .unwrap();
            if line == "日本語<Esc>" {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("keyboard input did not reach the host");

    engine.detach().await.unwrap();
    assert!(engine.client().is_none());
    assert!(
        process_temp.is_dir(),
        "detachment retains the process temporary directory"
    );
    // A caller may reattach before consuming the old welcome event. That event
    // must not reset the newly established attachment or its input worker.
    engine.connect("local_host", 80, 24).await.unwrap();
    assert_eq!(pid(&engine.client().unwrap()).await, host_pid);
    assert!(matches!(
        timeout(Duration::from_secs(5), engine.next_event())
            .await
            .unwrap(),
        EngineEvent::Frame(_)
    ));
    engine.detach().await.unwrap();
    engine.shutdown().await.unwrap();
    assert!(
        !process_temp.exists(),
        "shutdown removes the process temporary directory"
    );
    assert!(client.command("echo 'closed'").await.is_err());
    assert!(
        socket.exists(),
        "detachment preserves the host RPC endpoint"
    );
    drop(engine);
    assert!(!process_temp.exists());

    let mut engine = Engine::new(config(root.path(), &socket)).unwrap();
    engine.connect("local_host", 80, 24).await.unwrap();
    let client = engine.client().unwrap();
    assert_eq!(pid(&client).await, host_pid);
    let line = client
        .nvim()
        .request::<api::NvimGetCurrentLine, String>(())
        .await
        .unwrap();
    assert_eq!(line, "日本語<Esc>");
    wait_setup(&engine, &client).await;
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_reattaches_using_a_checkpoint_reference() {
    let root = tempdir().unwrap();
    let socket = root.path().join("checkpoint-reference.sock");
    let mut engine = Engine::new(session_config(root.path(), &socket, None)).unwrap();
    engine.connect("local_host", 80, 24).await.unwrap();

    let client = engine.client().unwrap();
    let host_pid = pid(&client).await;
    leave_inactive_tab(&client).await;
    let checkpoint = engine.checkpoint().unwrap();
    let grids = hidden_grids(checkpoint.state()).unwrap();
    engine.detach().await.unwrap();

    engine
        .connect_with_checkpoint_reference("local_host", 80, 24, checkpoint)
        .await
        .unwrap();
    let client = engine.client().unwrap();
    assert_eq!(pid(&client).await, host_pid);
    let first = next_frame(&mut engine).await;
    assert!(first.damage.full);
    assert_no_markers(&first);

    revisit_inactive_tab(&mut engine, &client, &grids).await;
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_restores_checkpoints_when_reattaching_and_switching_owned_hosts() {
    let root = tempdir().unwrap();
    let socket = root.path().join("checkpoint.sock");
    let startup =
        "lua assert(vim.env.NAUVIM_CONNECTION == 'local'); vim.env.NAUVIM_CONNECTION = nil";
    let mut engine = Engine::new(session_config(root.path(), &socket, Some(startup))).unwrap();
    engine.connect("local_host", 80, 24).await.unwrap();
    let client = engine.client().unwrap();
    assert!(lua_bool(&client, "return vim.env.NAUVIM_CONNECTION == nil").await);
    let host_pid = pid(&client).await;
    leave_inactive_tab(&client).await;
    let checkpoint = engine.checkpoint().unwrap();
    assert!(hidden_grids(checkpoint.state()).is_some());

    engine.detach().await.unwrap();
    assert!(engine.checkpoint().is_none());
    engine
        .connect_with_checkpoint("local_host", 80, 24, checkpoint.clone())
        .await
        .unwrap();
    assert_eq!(pid(&engine.client().unwrap()).await, host_pid);
    timeout(Duration::from_secs(5), async {
        while client
            .nvim()
            .request::<api::NvimEval, i64>(("getpid()",))
            .await
            .is_ok()
        {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the previous attachment did not close");
    let grids = restored_grids(&mut engine).await;

    engine.connect("next_host", 80, 24).await.unwrap();
    let other = engine.client().unwrap();
    assert_ne!(pid(&other).await, host_pid);
    assert_no_markers(&next_frame(&mut engine).await);
    let _ = timeout(Duration::from_secs(5), other.command("qa!"))
        .await
        .unwrap();
    assert_eq!(next_welcome(&mut engine).await, None);

    engine
        .connect_with_checkpoint("local_host", 80, 24, checkpoint.clone())
        .await
        .unwrap();
    let client = engine.client().unwrap();
    assert_eq!(pid(&client).await, host_pid);
    assert_eq!(restored_grids(&mut engine).await, grids);
    revisit_inactive_tab(&mut engine, &client, &grids).await;

    let _ = timeout(Duration::from_secs(5), client.command("qa!"))
        .await
        .unwrap();
    assert_eq!(next_welcome(&mut engine).await, None);
    let error = engine
        .connect_with_checkpoint("local_host", 80, 24, checkpoint)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.ends_with("no running session"), "{error}");
    assert_eq!(next_welcome(&mut engine).await, Some(error));
    assert!(engine.client().is_none());
    assert!(!socket.exists());

    engine.connect("local_host", 80, 24).await.unwrap();
    assert_ne!(pid(&engine.client().unwrap()).await, host_pid);
    assert_no_markers(&next_frame(&mut engine).await);
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_restores_an_inactive_tab_in_a_new_client_process() {
    use nauvim_core::Checkpoint;

    use nvim_rpc_ui::Limits;

    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    use std::process;

    const TEST: &str = "live_restores_an_inactive_tab_in_a_new_client_process";
    const ROOT: &str = "NAUVIM_CHECKPOINT_TEST_ROOT";
    const STAGE: &str = "NAUVIM_CHECKPOINT_TEST_STAGE";
    const PID_FILE: &str = "host-pid";
    const CHECKPOINT_FILE: &str = "checkpoint";
    const TEMP_DIR_FILE: &str = "client-temp-dir";

    if let Some(root) = env::var_os(ROOT) {
        let root = Path::new(&root);
        let socket = root.join("checkpoint.sock");
        let mut engine = Engine::new(session_config(root, &socket, None)).unwrap();

        match env::var(STAGE).as_deref() {
            Ok("prepare") => {
                engine.connect("local_host", 80, 24).await.unwrap();
                let client = engine.client().unwrap();
                let host_pid = pid(&client).await;
                fs::write(root.join(PID_FILE), host_pid.to_string()).unwrap();
                fs::write(
                    root.join(TEMP_DIR_FILE),
                    engine.temp_dir().as_os_str().as_encoded_bytes(),
                )
                .unwrap();
                leave_inactive_tab(&client).await;

                let checkpoint = engine.checkpoint().unwrap();
                assert!(hidden_grids(checkpoint.state()).is_some());
                fs::write(root.join(CHECKPOINT_FILE), checkpoint.encode().unwrap()).unwrap();
                process::exit(0);
            }
            Ok("restore") => {
                let bytes = fs::read(root.join(CHECKPOINT_FILE)).unwrap();
                let checkpoint =
                    Checkpoint::decode(&bytes, Limits::default(), bytes.len()).unwrap();
                engine
                    .connect_with_checkpoint("local_host", 80, 24, checkpoint)
                    .await
                    .unwrap();
                let client = engine.client().unwrap();
                let original = fs::read_to_string(root.join(PID_FILE)).unwrap();
                assert_eq!(pid(&client).await, original.parse::<i64>().unwrap());
                let grids = restored_grids(&mut engine).await;
                revisit_inactive_tab(&mut engine, &client, &grids).await;
                quit(&mut engine).await;
            }
            stage => panic!("unexpected checkpoint fixture stage {stage:?}"),
        }
        return;
    }

    let root = tempdir().unwrap();
    for stage in ["prepare", "restore"] {
        let mut child = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", TEST, "--nocapture"])
            .env(ROOT, root.path())
            .env(STAGE, stage)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let result = timeout(Duration::from_secs(30), child.wait()).await;
        if !matches!(&result, Ok(Ok(_))) {
            let _ = child.kill().await;
        }
        if stage == "prepare"
            && let Ok(dir) = fs::read(root.path().join(TEMP_DIR_FILE))
        {
            let _ = fs::remove_dir(OsString::from_vec(dir));
        }
        if !matches!(&result, Ok(Ok(status)) if status.success()) {
            if let Ok(host_pid) = fs::read_to_string(root.path().join(PID_FILE)) {
                let _ = Command::new("kill")
                    .args(["-TERM", &host_pid])
                    .status()
                    .await;
            }
            panic!("checkpoint fixture for {stage:?} failed: {result:?}");
        }
    }
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_discards_invalid_client_ids_before_validating_plugin_arguments() {
    let root = tempdir().unwrap();
    let mut engine = Engine::new(config(root.path(), &root.path().join("ids.sock"))).unwrap();
    engine.connect("local_host", 50, 12).await.unwrap();
    let client = engine.client().unwrap();
    let original_entries = fs::read_dir(engine.temp_dir()).unwrap().count();
    for id in [
        "nil".to_owned(),
        "false".to_owned(),
        "-1".to_owned(),
        "4294967296".to_owned(),
        "0.5".to_owned(),
        lua_string(&engine.client_id().to_string()),
        engine.client_id().get().wrapping_add(1).to_string(),
    ] {
        let src = format!(
            r#"
            local channel = vim.api.nvim_list_uis()[1].chan
            local ok, ret = pcall(vim.rpcrequest, channel,
                "nauvim-plugin/http.serve_file", {id}, false, false, "invalid", {{}})
            return ok and (ret == nil or ret == vim.NIL)
            "#
        );
        assert!(
            lua_bool(&client, &src).await,
            "client ID should have been ignored: {id}"
        );
    }
    assert!(
        lua_bool(
            &client,
            r#"
        local channel = vim.api.nvim_list_uis()[1].chan
        local ok, ret = pcall(vim.rpcrequest, channel, "nauvim-plugin/filesystem.copy")
        return ok and (ret == nil or ret == vim.NIL)
    "#
        )
        .await
    );
    assert_eq!(
        fs::read_dir(engine.temp_dir()).unwrap().count(),
        original_entries,
        "invalid requests must not create resources"
    );
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_replaces_a_stale_owned_socket_and_preserves_non_socket_entries() {
    use std::os::unix::net::UnixListener;

    let root = tempdir().unwrap();
    let socket = root.path().join("stale.sock");
    drop(UnixListener::bind(&socket).unwrap());
    let mut engine = Engine::new(config(root.path(), &socket)).unwrap();
    engine.connect("local_host", 50, 12).await.unwrap();
    assert!(pid(&engine.client().unwrap()).await > 0);
    quit(&mut engine).await;

    let occupied = root.path().join("ordinary-file");
    fs::write(&occupied, "must be preserved").unwrap();
    let mut engine = Engine::new(config(root.path(), &occupied)).unwrap();
    let err = engine.connect("local_host", 50, 12).await.err().unwrap();
    assert!(err.to_string().contains("not a socket"));
    assert_eq!(fs::read_to_string(occupied).unwrap(), "must be preserved");
    assert!(engine.client().is_none());
    engine.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_host_functions_receive_host_role_information() {
    use nauvim_core::HostInfo;

    let root = tempdir().unwrap();
    let file = root.path().join("config.lua");
    fs::write(&file, "return {}").unwrap();
    let bin = env::var("NVIM_BIN").expect("set NVIM_BIN");
    let socket = root.path().join("functions.sock");
    let src = format!(
        r#"
        local client = require("nauvim-client-info")
        assert(client.config_file ~= nil)
        local function host(info)
            assert(info.config_file == nil)
            assert(info.user == client.user and info.host_name == client.host_name)
            assert(info.home_dir:sub(1,1) == "/")
            assert(info.project_dirs.cache_dir:sub(1,1) == "/")
        end
        return {{hosts = {{local_host = {{
            nvim = function(info) host(info); return {{prog = {}, args = {{"-u", "NONE", "-i", "NONE", "-n"}}, env = {{}}}} end,
            socket = function(info) host(info); return {} end,
        }}}}}}
        "#,
        lua_string(&bin),
        lua_string(socket.to_str().unwrap())
    );
    let (config, ()) = Config::evaluate(
        &src,
        HostInfo::local(Some(&file)).unwrap(),
        "org.example.nauvim.session-test",
        |_| Ok(()),
    )
    .unwrap();
    let mut engine = Engine::new(config).unwrap();
    engine.connect("local_host", 50, 12).await.unwrap();
    assert!(pid(&engine.client().unwrap()).await > 0);
    assert!(!next_frame(&mut engine).await.state.grids.is_empty());
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_plugin_connect_emits_welcome_then_destination_and_keeps_host_alive() {
    let root = tempdir().unwrap();
    let socket = root.path().join("switch.sock");
    let mut engine = Engine::new(config(root.path(), &socket)).unwrap();
    engine.connect("local_host", 50, 12).await.unwrap();
    let client = engine.client().unwrap();
    let host_pid = pid(&client).await;
    let client_id = engine.client_id();
    wait_setup(&engine, &client).await;
    leave_inactive_tab(&client).await;
    let checkpoint = engine.checkpoint().unwrap();
    let _ = timeout(
        Duration::from_secs(5),
        client
            .nvim()
            .request::<api::NvimExecLua, Object>((
                &format!("vim.rpcnotify(vim.api.nvim_list_uis()[1].chan, 'nauvim-plugin/client.connect', {}, 'next-host'); vim.cmd('detach')", client_id),
                (),
            )),
    )
    .await
    .expect("plugin session-switch invocation did not finish");
    timeout(Duration::from_secs(5), async {
        loop {
            match engine.next_event().await {
                EngineEvent::Welcome { error: None } => break,
                EngineEvent::Frame(_) => {}
                EngineEvent::Error(e) if is_clipboard_detection_error(&e) => {}
                EngineEvent::Welcome { error: Some(e) } | EngineEvent::Error(e) => {
                    panic!("switch failed: {e}")
                }
                _ => panic!("switch destination arrived before the welcome page"),
            }
        }
    })
    .await
    .expect("session switch did not return to the welcome page");
    assert!(matches!(
        timeout(Duration::from_secs(5), engine.next_event()).await.unwrap(),
        EngineEvent::Connect {host} if host == "next-host"
    ));
    assert!(engine.client().is_none());
    assert!(engine.temp_dir().is_dir());
    engine
        .connect_with_checkpoint("local_host", 50, 12, checkpoint)
        .await
        .unwrap();
    assert_eq!(engine.client_id(), client_id);
    let client = engine.client().unwrap();
    assert_eq!(pid(&client).await, host_pid);
    restored_grids(&mut engine).await;
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_session_switch_connects_without_application_connection_management() {
    let root = tempdir().unwrap();
    let mut engine = Engine::new(config(root.path(), &root.path().join("switch.sock"))).unwrap();
    engine.connect("local_host", 50, 12).await.unwrap();
    engine.resize(70, 18).await.unwrap();
    let client = engine.client().unwrap();
    let first_pid = pid(&client).await;
    let src = format!(
        "vim.rpcnotify(vim.api.nvim_list_uis()[1].chan, 'nauvim-plugin/client.connect', {}, 'next_host'); vim.cmd('detach')",
        engine.client_id()
    );
    let _ = timeout(
        Duration::from_secs(5),
        client
            .nvim()
            .request::<api::NvimExecLua, Object>((src.as_str(), ())),
    )
    .await
    .unwrap();
    assert_eq!(next_welcome(&mut engine).await, None);
    assert!(
        matches!(engine.next_event().await, EngineEvent::Connect {host} if host == "next_host")
    );
    let _ = next_frame(&mut engine).await;
    let client = engine.client().unwrap();
    assert_ne!(pid(&client).await, first_pid);
    assert!(
        lua_bool(
            &client,
            "local ui=vim.api.nvim_list_uis()[1]; return ui.width==70 and ui.height==18"
        )
        .await
    );
    let _ = timeout(Duration::from_secs(5), client.command("qa!")).await;
    assert_eq!(next_welcome(&mut engine).await, None);
    engine.connect("local_host", 50, 12).await.unwrap();
    assert_eq!(pid(&engine.client().unwrap()).await, first_pid);
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_host_detach_and_quit_from_another_rpc_client_return_to_welcome() {
    use nvim_rpc_plugin::{ConnectionOptions, NoHandler, connect};

    use tokio::net::UnixStream;

    let root = tempdir().unwrap();
    let socket = root.path().join("host-lifecycle.sock");
    let mut engine = Engine::new(config(root.path(), &socket)).unwrap();
    engine.connect("local_host", 50, 12).await.unwrap();
    let client = engine.client().unwrap();
    let host_pid = pid(&client).await;
    let _ = timeout(Duration::from_secs(5), client.command("detach"))
        .await
        .expect("host :detach invocation did not finish");
    assert_eq!(next_welcome(&mut engine).await, None);
    assert!(engine.client().is_none());
    assert!(engine.temp_dir().is_dir());
    engine.connect("local_host", 50, 12).await.unwrap();
    assert_eq!(pid(&engine.client().unwrap()).await, host_pid);

    let observer = UnixStream::connect(&socket).await.unwrap();
    let (reader, writer) = observer.into_split();
    let mut observer = connect(reader, writer, NoHandler, ConnectionOptions::default()).unwrap();
    let _ = timeout(
        Duration::from_secs(5),
        observer.client.request::<api::NvimCommand, Object>(("q",)),
    )
    .await
    .expect("host :q invocation did not finish");
    assert_eq!(next_welcome(&mut engine).await, None);
    assert!(engine.client().is_none());
    assert!(engine.temp_dir().is_dir());
    let _ = observer.close_and_wait().await;

    engine.connect("local_host", 50, 12).await.unwrap();
    assert_ne!(pid(&engine.client().unwrap()).await, host_pid);
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6, local Unix sockets and kill utility"]
async fn live_killed_reattached_host_reports_an_error_and_preserves_engine() {
    let root = tempdir().unwrap();
    let socket = root.path().join("killed-reattached.sock");
    let mut creator = Engine::new(config(root.path(), &socket)).unwrap();
    creator.connect("local_host", 50, 12).await.unwrap();
    let host_pid = pid(&creator.client().unwrap()).await;
    creator.shutdown().await.unwrap();
    drop(creator);
    let mut engine = Engine::new(config(root.path(), &socket)).unwrap();
    engine.connect("local_host", 50, 12).await.unwrap();
    assert_eq!(pid(&engine.client().unwrap()).await, host_pid);
    assert!(
        Command::new("kill")
            .args(["-KILL", &host_pid.to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    let error = next_welcome(&mut engine).await;
    assert!(
        error.is_some(),
        "unexpected host termination must display an error"
    );
    assert!(engine.client().is_none());
    assert!(engine.temp_dir().is_dir());
    engine.connect("local_host", 50, 12).await.unwrap();
    assert_ne!(pid(&engine.client().unwrap()).await, host_pid);
    quit(&mut engine).await;
}

#[tokio::test]
#[ignore = "requires NVIM_BIN pointing to Neovim v0.12.6 and local Unix sockets"]
async fn live_structured_shell_program_preserves_environment_and_argument_suffix() {
    use std::str;

    let root = tempdir().unwrap();
    let init = root.path().join("init.lua");
    let socket = root.path().join("literal ' socket.sock");
    let record = root.path().join("argv record");
    let bin = env::var("NVIM_BIN").expect("Set NVIM_BIN");
    let script = r#"printf '%s\0' "$@" > "$NAUVIM_ARGV_FILE"; exec "$@""#;
    let literal = "literal $HOME; 'quoted' \"double\" 日本語";
    fs::write(
        &init,
        "vim.g.nauvim_test_connection = os.getenv('NAUVIM_CONNECTION')",
    )
    .unwrap();
    let src = format!(
        r#"return {{hosts = {{local_host = {{
        nvim = {{prog = "sh", args = {{"-c", {}, "nauvim", {}, "-u", {}, "-i", "NONE", "-n", "-O3"}},
            env = {{NAUVIM_ARGV_FILE = {}, NAUVIM_LITERAL = {}, NAUVIM_CONNECTION = "configured"}}}},
        socket = {},
    }}}}}}"#,
        lua_string(script),
        lua_string(&bin),
        lua_string(init.to_str().unwrap()),
        lua_string(record.to_str().unwrap()),
        lua_string(literal),
        lua_string(socket.to_str().unwrap())
    );
    let file = root.path().join("config.lua");
    fs::write(&file, src).unwrap();
    let (config, ()) =
        Config::load(Some(&file), "org.example.nauvim.session-test", |_| Ok(())).unwrap();
    let mut engine = Engine::new(config).unwrap();
    engine.connect("local_host", 80, 24).await.unwrap();
    let client = engine.client().unwrap();
    let value = client
        .nvim()
        .request::<api::NvimExecLua, String>(("return vim.env.NAUVIM_LITERAL", ()))
        .await
        .unwrap();
    assert_eq!(value, literal);
    assert!(
        lua_bool(&client, "return vim.g.nauvim_test_connection == 'local'").await,
        "local init.lua must see the launch marker through the wrapper"
    );

    let recorded = fs::read(&record).unwrap();
    let actual: Vec<_> = recorded
        .split(|&b| b == 0)
        .filter(|v| !v.is_empty())
        .map(|v| str::from_utf8(v).unwrap())
        .collect();
    assert_eq!(
        actual,
        [
            bin.as_str(),
            "-u",
            init.to_str().unwrap(),
            "-i",
            "NONE",
            "-n",
            "-O3",
            "--headless",
            "--listen",
            socket.to_str().unwrap()
        ]
    );
    let windows = client
        .nvim()
        .request::<api::NvimExecLua, usize>(("return #vim.api.nvim_list_wins()", ()))
        .await
        .unwrap();
    assert_eq!(windows, 3);
    quit(&mut engine).await;
}
