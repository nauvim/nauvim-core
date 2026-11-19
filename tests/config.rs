use nauvim_core::{Config, Error, HostInfo, HostSelection, ProjectDirs};

use tempfile::tempdir;

use std::fs;
use std::path::Path;

fn info(file: &Path) -> HostInfo {
    let root = file.parent().unwrap();
    HostInfo {
        host_name: "client.example".into(),
        user: "tester".into(),
        config_file: Some(file.to_path_buf()),
        home_dir: root.join("home"),
        project_dirs: ProjectDirs {
            cache_dir: root.join("cache"),
            data_dir: root.join("data"),
            runtime_dir: root.join("runtime"),
        },
    }
}

fn evaluate(src: &str) -> Result<Config, Error> {
    Config::evaluate(src, info(Path::new("/tmp/nauvim-tests/config.lua")))
}

#[test]
fn evaluates_client_information_defaults_and_ordered_configuration() {
    let config = evaluate(
        r#"
        local client = require("nauvim-client-info")
        assert(client.host_name == "client.example")
        assert(client.user == "tester")
        assert(client.config_file == "/tmp/nauvim-tests/config.lua")
        assert(client.home_dir == "/tmp/nauvim-tests/home")
        assert(client.project_dirs.cache_dir == "/tmp/nauvim-tests/cache")
        assert(client.project_dirs.data_dir == "/tmp/nauvim-tests/data")
        assert(client.project_dirs.runtime_dir == "/tmp/nauvim-tests/runtime")
        return {
            hosts = {
                ["z-host"] = { nvim = "spaced nvim", socket = "relative socket" },
                ["a-host"] = {},
            },
            default_host = "a-host",
            fonts = { {family = "Mono One", size = 13}, {family = "Mono Two"} },
        }
        "#,
    )
    .unwrap();
    assert_eq!(config.hosts().collect::<Vec<_>>(), ["a-host", "z-host"]);
    assert_eq!(config.default_host(), Some("a-host"));
    assert_eq!(
        config
            .fonts()
            .iter()
            .map(|f| (f.family.as_str(), f.size))
            .collect::<Vec<_>>(),
        [("Mono One", 13), ("Mono Two", 12)]
    );
    let config = evaluate("return {}").unwrap();
    assert_eq!(config.hosts().collect::<Vec<_>>(), Vec::<&str>::new());
    assert_eq!(config.startup_host(), Some(HostSelection::Default));
    assert_eq!(config.default_host(), None);
}

#[test]
fn retains_function_settings_without_evaluating_unused_hosts() {
    let config = evaluate(
        r#"
        return {
            hosts = {
                unused = {
                    nvim = function(info) error("unused nvim callback") end,
                    socket = function(info) error("unused socket callback") end,
                },
            },
        }
        "#,
    )
    .unwrap();
    assert_eq!(config.hosts().collect::<Vec<_>>(), ["unused"]);
}

#[test]
fn loads_local_lua_modules_from_each_configuration_parent() {
    let dir = tempdir().unwrap();
    for relative in ["init.lua", "custom.lua", "nested/init.lua"] {
        let file = dir.path().join(relative);
        let root = file.parent().unwrap();
        fs::create_dir_all(root.join("lua/foo/bar")).unwrap();
        fs::create_dir_all(root.join("lua/other/module")).unwrap();
        fs::write(
            root.join("lua/foo/bar.lua"),
            "return 'file-before-directory'",
        )
        .unwrap();
        fs::write(
            root.join("lua/foo/bar/init.lua"),
            "error('directory module must follow file module')",
        )
        .unwrap();
        fs::write(
            root.join("lua/other/module/init.lua"),
            "return {default_host = 'from-local-lua', hosts = {['from-local-lua'] = {}}}",
        )
        .unwrap();
        fs::write(root.join("legacy_only.lua"), "return true").unwrap();
        fs::write(
            &file,
            r#"
            local info = require("nauvim-client-info")
            local root = assert(info.config_file:match("^(.*)/[^/]+$"))
            local first, second, rest = package.path:match("^([^;]+);([^;]+);(.*)$")
            assert(first == root .. "/lua/?.lua")
            assert(second == root .. "/lua/?/init.lua")
            assert(#rest > 0)
            assert(require("foo.bar") == "file-before-directory")
            assert(not pcall(require, "legacy_only"))
            return require("other.module")
            "#,
        )
        .unwrap();
        let config = Config::load(Some(&file)).unwrap();
        assert_eq!(config.default_host(), Some("from-local-lua"), "{file:?}");
    }
}

#[test]
fn retains_existing_lua_search_paths_after_local_templates() {
    use mlua::{Lua, Table};

    let dir = tempdir().unwrap();
    let file = dir.path().join("custom.lua");
    let lua = Lua::new();
    let package: Table = lua.globals().get("package").unwrap();
    let original: String = package.get("path").unwrap();
    let expected = format!(
        "{}/lua/?.lua;{}/lua/?/init.lua;{original}",
        dir.path().display(),
        dir.path().display(),
    );
    let src = format!(
        "assert(package.path == {}); return {{}}",
        lua_quote(&expected),
    );
    Config::evaluate(&src, info(&file)).unwrap();
}

#[test]
fn loads_default_init_in_an_isolated_process() {
    use std::env;
    use std::process::Command;

    const CHILD: &str = "NAUVIM_TEST_DEFAULT_INIT_CHILD";
    if env::var(CHILD).as_deref() == Ok("1") {
        let config = Config::load(None).unwrap();
        assert_eq!(config.default_host(), Some("default-init"));
        return;
    }
    let dir = tempdir().unwrap();
    let root = dir.path().join("nauvim");
    fs::create_dir_all(root.join("lua/settings")).unwrap();
    fs::write(root.join("init.lua"), "return require('settings.hosts')").unwrap();
    fs::write(
        root.join("lua/settings/hosts.lua"),
        "return {default_host = 'default-init', hosts = {['default-init'] = {}}}",
    )
    .unwrap();
    fs::write(root.join("config.lua"), "error('legacy default filename')").unwrap();
    let output = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "loads_default_init_in_an_isolated_process",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("XDG_CONFIG_HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "default configuration child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn lua_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('\"', "\\\"")
            .replace('\n', "\\n")
    )
}

#[test]
fn resolves_config_symlinks_before_selecting_the_module_root() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let real = dir.path().join("real");
    let linked = dir.path().join("linked");
    fs::create_dir_all(real.join("lua")).unwrap();
    fs::create_dir_all(linked.join("lua")).unwrap();
    fs::write(
        real.join("lua/sibling.lua"),
        "return {hosts = {resolved = {}}}",
    )
    .unwrap();
    fs::write(linked.join("lua/sibling.lua"), "error('wrong module root')").unwrap();
    let file = real.join("config.lua");
    fs::write(&file, "return require('sibling')").unwrap();
    symlink(&file, linked.join("config.lua")).unwrap();
    let config = Config::load(Some(&linked.join("config.lua"))).unwrap();
    assert_eq!(config.hosts().collect::<Vec<_>>(), ["resolved"]);
    let client = HostInfo::local(Some(&linked.join("config.lua"))).unwrap();
    assert_eq!(client.config_file, Some(fs::canonicalize(file).unwrap()));
    for path in [
        client.home_dir,
        client.project_dirs.cache_dir,
        client.project_dirs.data_dir,
        client.project_dirs.runtime_dir,
    ] {
        assert!(path.is_absolute(), "{path:?}");
    }
    assert_eq!(HostInfo::local(None).unwrap().config_file, None);
}

#[test]
fn rejects_missing_files_and_invalid_configurations() {
    let dir = tempdir().unwrap();
    let config = Config::load(Some(&dir.path().join("missing.lua"))).unwrap();
    assert!(config.hosts().next().is_none());
    assert_eq!(config.startup_host(), Some(HostSelection::Default));
    assert_eq!(config.diagnostics().len(), 1);
    for src in [
        "return nil",
        "return {hosts = {a = {default = true}, b = {default = true}}}",
        "return {['default-host'] = 'a'}",
        "return {hosts = {a = {['nvim-bin'] = 'nvim'}}}",
        "return {hosts = {a = {nvim = false}}}",
        "return {hosts = {a = {socket = 42}}}",
        "return {hosts = {a = {args = {'-O3'}}}}",
        "return {hosts = {a = {config = 'NONE'}}}",
        "return {hosts = {a = {policies = {['local-forward'] = false}}}}",
        "return {fonts = {{size = 0}}}",
        "return {fonts = {{family = 'Mono', size = 12.0}}}",
        "return {default_host = ''}",
        "return {default_host = 42}",
        "return {fonts = {{size = -1}}}",
        "return {fonts = {{size = math.huge}}}",
    ] {
        assert!(evaluate(src).is_err(), "configuration was accepted: {src}");
    }
    let mut missing = info(Path::new("/tmp/config.lua"));
    missing.config_file = None;
    assert!(
        Config::evaluate("error('must not execute')", missing)
            .err()
            .unwrap()
            .to_string()
            .contains("config_file")
    );
}

#[cfg(feature = "ssh")]
#[test]
fn validates_unused_native_ssh_tables_and_inline_jump_hosts() {
    for field in [
        "unknown = true",
        "request_tty = true",
        "host_name = false",
        "port = -1",
        "port = 65536",
        "port = '2.5'",
        "identity_file = 'relative'",
        "identity_file = {'/absolute', 'relative'}",
        "identity_agent = 'none'",
        "certificate_file = ''",
        "identities_only = 'sometimes'",
        "proxy_jump = {{host_name = 'jump', unknown = true}}",
        "proxy_jump = {{host_name = 'jump', identity_file = 'relative'}}",
    ] {
        let src = format!("return {{hosts = {{unused = {{ssh = {{{field}}}}}}}}}");
        assert!(
            evaluate(&src).is_err(),
            "native SSH setting accepted: {field}"
        );
    }
    for value in ["true", "false", "'yes'", "'NO'", "'True'", "'FALSE'"] {
        let src = format!(
            "return {{hosts = {{['literal*?!Alias'] = {{ssh = {{port = '0', identities_only = {value}, identity_file = '/missing/id', identity_agent = '/missing/agent'}}}}}}}}"
        );
        assert!(
            evaluate(&src).is_ok(),
            "native SSH boolean rejected: {value}"
        );
    }
}

#[test]
fn accepts_user_defined_host_aliases_with_punctuation() {
    let config = evaluate("return {hosts = {['prod-host*?!'] = {}}}").unwrap();
    assert_eq!(config.hosts().collect::<Vec<_>>(), ["prod-host*?!"]);
}

#[test]
fn selects_startup_using_top_level_default_host_and_preserves_independent_default() {
    for src in ["return {}", "return {hosts=nil}", "return {hosts={}}"] {
        let config = evaluate(src).unwrap();
        assert_eq!(config.startup_host(), Some(HostSelection::Default), "{src}");
        assert!(config.hosts().next().is_none());
    }
    let config = evaluate("return {hosts={['local']={},['default']={}}}").unwrap();
    assert_eq!(config.startup_host(), None);
    assert_eq!(config.hosts().collect::<Vec<_>>(), ["default", "local"]);
    for src in [
        "return {default_host='work'}",
        "return {default_host='work',hosts={work={}}}",
        "return {default_host='work',hosts={other={}}}",
    ] {
        let config = evaluate(src).unwrap();
        assert_eq!(
            config.startup_host(),
            Some(HostSelection::Alias("work")),
            "{src}"
        );
    }
}

#[test]
fn accepts_font_shorthands_and_integer_size_defaults() {
    for src in [
        "return {fonts='Mono'}",
        "return {fonts={family='Mono'}}",
        "return {fonts={{family='Mono',size=nil}}}",
    ] {
        let config = evaluate(src).unwrap();
        assert_eq!(config.fonts().len(), 1);
        assert_eq!(config.fonts()[0].family, "Mono");
        assert_eq!(config.fonts()[0].size, 12);
    }
    for src in [
        "return {fonts={{family='Mono',size=12.5}}}",
        "return {fonts={'Mono'}}",
        "return {fonts={family=123}}",
        "return {fonts={[2]={family='Mono'}}}",
    ] {
        assert!(evaluate(src).is_err(), "{src}");
    }
}

#[test]
fn falls_back_to_complete_defaults_after_read_execution_or_validation_failure() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("init.lua");
    for src in [
        "return {",
        "error('broken setup')",
        "return {default_host=''}",
        "return {hosts={custom={}},fonts={family='Mono',size=12.0}}",
    ] {
        fs::write(&file, src).unwrap();
        let config = Config::load(Some(&file)).unwrap();
        assert_eq!(config.startup_host(), Some(HostSelection::Default), "{src}");
        assert!(config.hosts().next().is_none());
        assert_eq!(config.fonts()[0].family, "monospace");
        assert_eq!(config.diagnostics().len(), 1);
    }
}

#[test]
fn validates_global_ssh_paths_without_accessing_them() {
    for field in ["config", "known_hosts"] {
        for value in ["''", "'relative/config'", "42", "false"] {
            assert!(evaluate(&format!("return {{ssh={{{field}={value}}}}}")).is_err());
        }
        assert!(
            evaluate(&format!(
                "return {{ssh={{{field}='/nonexistent/nauvim-test'}}}}"
            ))
            .is_ok()
        );
    }
}
