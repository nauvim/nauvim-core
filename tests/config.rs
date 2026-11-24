use nauvim_core::{
    Config, Error, HostInfo, HostSelection, ProjectDirs, UiConfig, UiTableId, UiValue,
};

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

const UI_ID: &str = "org.example.nauvim.test";

fn evaluate(src: &str) -> Result<Config, Error> {
    Config::evaluate(
        src,
        info(Path::new("/tmp/nauvim-tests/config.lua")),
        UI_ID,
        |_| Ok(()),
    )
    .map(|(config, ())| config)
}

#[test]
fn evaluates_client_information_defaults_and_ordered_configuration() {
    let config = evaluate(
        r#"
        local client = require("nauvim-client-info")
        assert(client.ui_id == "org.example.nauvim.test")
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
        }
        "#,
    )
    .unwrap();
    assert_eq!(config.hosts().collect::<Vec<_>>(), ["a-host", "z-host"]);
    assert_eq!(config.default_host(), Some("a-host"));
    let config = evaluate("return {}").unwrap();
    assert_eq!(config.hosts().collect::<Vec<_>>(), Vec::<&str>::new());
    assert_eq!(config.startup_host(), Some(HostSelection::Default));
    assert_eq!(config.default_host(), None);
}

#[test]
fn exposes_client_platform_during_configuration() {
    use std::env::consts;

    let src = format!(
        r#"
        local client = require("nauvim-client-info")
        assert(client.os == {})
        assert(client.os_family == {})
        assert(client.arch == {})
        return {{}}
        "#,
        lua_quote(consts::OS),
        lua_quote(consts::FAMILY),
        lua_quote(consts::ARCH),
    );

    evaluate(&src).unwrap();
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
        let config = Config::load(Some(&file), UI_ID, |_| Ok(())).unwrap().0;
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
    Config::evaluate(&src, info(&file), UI_ID, |_| Ok(())).unwrap();
}

#[test]
fn loads_default_init_in_an_isolated_process() {
    use std::env;
    use std::process::Command;

    const CHILD: &str = "NAUVIM_TEST_DEFAULT_INIT_CHILD";
    if env::var(CHILD).as_deref() == Ok("1") {
        let config = Config::load(None, UI_ID, |_| Ok(())).unwrap().0;
        assert_eq!(config.startup_host(), Some(HostSelection::Default));
        assert!(config.diagnostics().is_empty());

        let root = Path::new(&env::var_os("XDG_CONFIG_HOME").unwrap()).join("nauvim");
        fs::write(root.join("init.lua"), "return require('settings.hosts')").unwrap();
        let config = Config::load(None, UI_ID, |_| Ok(())).unwrap().0;
        assert_eq!(config.default_host(), Some("default-init"));
        return;
    }
    let dir = tempdir().unwrap();
    let root = dir.path().join("nauvim");
    fs::create_dir_all(root.join("lua/settings")).unwrap();
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
    let config = Config::load(Some(&linked.join("config.lua")), UI_ID, |_| Ok(()))
        .unwrap()
        .0;
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
    let config = Config::load(Some(&dir.path().join("missing.lua")), UI_ID, |_| Ok(()))
        .unwrap()
        .0;
    assert!(config.hosts().next().is_none());
    assert_eq!(config.startup_host(), Some(HostSelection::Default));
    assert_eq!(config.diagnostics().len(), 1);
    for src in ["return nil", "return {default_host = ''}"] {
        assert!(evaluate(src).is_err(), "configuration was accepted: {src}");
    }
    let mut missing = info(Path::new("/tmp/config.lua"));
    missing.config_file = None;
    assert!(
        Config::evaluate("error('must not execute')", missing, UI_ID, |_| Ok(()))
            .err()
            .unwrap()
            .to_string()
            .contains("config_file")
    );
}

#[test]
fn recovers_unsupported_configuration_keys_and_types() {
    let src = r#"return {
        default_host = 42, ['default-host'] = "unused", [false] = true, unknown = true,
        clipboard = {backend = false, timeout_ms = 25},
        hosts = {
            ignored = false, [23] = {},
            kept = {['nvim-bin'] = 'other',socket = 42,nvim = false,policies={['local-forward']=true}},
        },
        ssh = {config=false, known_hosts='/absolute', unknown='ignored'},
    }"#;
    let config = evaluate(src).unwrap();
    assert_eq!(config.default_host(), None);
    assert_eq!(config.startup_host(), None);
    assert_eq!(config.hosts().collect::<Vec<_>>(), ["kept"]);
    assert_eq!(config.clipboard().backend(), "auto");
    assert_eq!(config.clipboard().timeout().as_millis(), 25);
}

#[cfg(feature = "ssh")]
#[test]
fn validates_unused_native_ssh_tables_and_inline_jump_hosts() {
    for field in [
        "port = -1",
        "port = 65536",
        "port = '2.5'",
        "identity_file = 'relative'",
        "identity_agent = 'none'",
        "certificate_file = ''",
        "identities_only = 'sometimes'",
        "proxy_jump = {{host_name = 'jump', identity_file = 'relative'}}",
    ] {
        let src = format!("return {{hosts = {{unused = {{ssh = {{{field}}}}}}}}}");
        assert!(
            evaluate(&src).is_err(),
            "native SSH setting accepted: {field}"
        );
    }
    for field in [
        "unknown = true",
        "request_tty = true",
        "host_name = false",
        "identity_file = {'/absolute', 'relative'}",
        "proxy_jump = {{host_name = 'jump', unknown = true}}",
    ] {
        let src = format!("return {{hosts = {{unused = {{ssh = {{{field}}}}}}}}}");
        assert!(evaluate(&src).is_ok(), "{field}");
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
fn validates_typed_configuration_strings() {
    for src in [
        r"return {default_host='\239\187\191alias'}",
        r"return {hosts={['al\0ias']={}}}",
        r"return {hosts={a={nvim='\239\187\191nvim'}}}",
        r"return {hosts={a={nvim={prog='nv\0im'}}}}",
        r"return {hosts={a={nvim={args={'\255'}}}}}",
        r"return {hosts={a={nvim={env={['\239\187\191name']='value'}}}}}",
        r"return {hosts={a={nvim={env={NAME='val\0ue'}}}}}",
        r"return {hosts={a={socket='so\0cket'}}}",
        r"return {ssh={config='/ssh/con\0fig'}}",
        r"return {hosts={a={ssh={host_name='\255'}}}}",
        r"return {hosts={a={ssh={proxy_jump={{user='\239\187\191user'}}}}}}",
        r"return {hosts={a={ssh={identity_file='/ssh/\0id'}}}}",
        r"return {clipboard='au\0to'}",
        r"return {clipboard={backend='\239\187\191auto'}}",
    ] {
        let error = evaluate(src).err().unwrap().to_string();
        assert!(
            error.contains("U+FEFF or contain NUL") || error.contains("valid UTF-8"),
            "{src}: {error}"
        );
    }
    assert!(evaluate(r"return {default_host='al\239\187\191ias',hosts={['al\239\187\191ias']={nvim={args={'', 'in\239\187\191terior'}},socket='in\239\187\191terior'}}}").is_ok());
}

#[test]
fn validates_host_information_before_running_configuration() {
    for field in ["host_name", "user"] {
        let mut info = info(Path::new("/tmp/nauvim-tests/config.lua"));
        match field {
            "host_name" => info.host_name = "\u{feff}host".into(),
            "user" => info.user = "us\0er".into(),
            _ => unreachable!(),
        }
        let error = Config::evaluate("error('configuration ran')", info, UI_ID, Ok)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(field), "{error}");
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
fn falls_back_to_complete_defaults_after_read_execution_or_validation_failure() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("init.lua");
    for src in [
        "return {",
        "error('broken setup')",
        "return {default_host=''}",
        "return {hosts={custom={}},ui_opts={unused={value={function() end}}}}",
    ] {
        fs::write(&file, src).unwrap();
        let config = Config::load(Some(&file), UI_ID, |_| Ok(())).unwrap().0;
        assert_eq!(config.startup_host(), Some(HostSelection::Default), "{src}");
        assert!(config.hosts().next().is_none());
        assert!(config.client_info().is_none());
        assert_eq!(config.default_host(), None);
        assert_eq!(config.diagnostics().len(), 1);
    }
}

#[test]
fn validates_global_ssh_paths_without_accessing_them() {
    for field in ["config", "known_hosts"] {
        for value in ["''", "'relative/config'"] {
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

#[test]
fn validates_plugin_policies_for_unselected_hosts() {
    for (policy, diagnostic) in [
        (
            "{plugin={local_forward={tcp='any'}}}",
            "plugin.local_forward.tcp must be a list",
        ),
        (
            "{plugin={local_forward={tcp={{host_addr='example.test',host_port=80}}}}}",
            "client_port must be an integer",
        ),
        (
            "{plugin={local_forward={tcp={{client_port=65536,host_addr='example.test',host_port=80}}}}}",
            "client_port must be an integer",
        ),
        (
            "{plugin={local_forward={tcp={{client_port=0,host_addr='example.test',host_port=0}}}}}",
            "host_port must be an integer",
        ),
        (
            r"{plugin={local_forward={tcp={{client_port=0,host_addr='\255',host_port=80}}}}}",
            "host_addr must be valid UTF-8",
        ),
        (
            "{plugin={local_forward={socket={{client_socket_path='/a.sock',host_socket_path='/a.sock'}}}}}",
            "client_socket_path must be relative",
        ),
        (
            "{plugin={local_forward={socket={{client_socket_path='a.sock',host_socket_path='a.sock'}}}}}",
            "host_socket_path must be an absolute POSIX path",
        ),
        (
            "{plugin={http={open_webview={origins={[2]='https://example.test'}}}}}",
            "origins must be a list indexed consecutively from 1",
        ),
        (
            "{plugin={http={open_webview={origins={example='https://example.test'}}}}}",
            "origins must be a list indexed consecutively from 1",
        ),
    ] {
        let source = format!(
            "return {{default_host='selected', hosts={{selected={{}},unused={{policies={policy}}}}}}}"
        );
        let error = evaluate(&source).err().unwrap().to_string();
        assert!(error.contains(diagnostic), "{source}: {error}");
    }
}

#[test]
fn validates_webview_origin_entries() {
    for origin in [
        "'https://example.test/'",
        "'https://example.test/guide'",
        "'https://example.test?q=value'",
        "'https://example.test#section'",
        "'https://user@example.test'",
        "'https://:secret@example.test'",
        "'https://example.test:'",
        "'https://example.test:65536'",
        "'https:// example.test'",
        "'https:example.test'",
        "'https:///example.test'",
        "'ftp://example.test'",
        "false",
    ] {
        let source = format!(
            r"return {{
                hosts = {{ unused = {{
                    policies = {{ plugin = {{ http = {{
                        open_webview = {{ origins = {{{origin}}} }},
                    }}}}}},
                }}}},
            }}"
        );
        let error = evaluate(&source).err().unwrap().to_string();
        assert!(
            error.contains("Webview origin policy entry"),
            "{source}: {error}"
        );
    }
}

fn ui_options(src: &str) -> Result<UiConfig, Error> {
    Config::evaluate(
        src,
        info(Path::new("/tmp/nauvim-tests/config.lua")),
        UI_ID,
        Ok,
    )
    .map(|(_, opts)| opts)
}

#[test]
fn supplies_empty_ui_options_for_defaults_and_missing_entries() {
    for src in [
        "return {}",
        "return {ui_opts=nil,use_ui_opts=nil}",
        "return {ui_opts={}}",
        "return {ui_opts={other={value=true}}}",
        "return {ui_opts={['org.example.nauvim.test']=nil}}",
        "return {use_ui_opts='missing',ui_opts={['org.example.nauvim.test']={value=true}}}",
        "return {use_ui_opts='missing',ui_opts={missing=nil,['org.example.nauvim.test']={value=true}}}",
    ] {
        let opts = ui_options(src).unwrap();
        assert_eq!(opts.root().entries().len(), 0, "{src}");
    }
}

#[test]
fn selects_exact_ui_key_without_merging_dictionaries() {
    let options = r#"{
        ['org.example.nauvim.test']={font='Mono'},
        ['org.example.nauvim.Test']={font='other case'},
        ['some-alias']={color='red'},
    }"#;
    for (selector, key, expected) in [
        ("nil", "font", "Mono"),
        ("'some-alias'", "color", "red"),
        ("'org.example.nauvim.Test'", "font", "other case"),
    ] {
        let src = format!("return {{ui_opts={options},use_ui_opts={selector}}}");
        let opts = ui_options(&src).unwrap();
        assert_eq!(opts.root().entries().len(), 1, "{src}");
        assert_eq!(
            opts.get(key),
            Some(&UiValue::String(expected.into())),
            "{src}"
        );
    }
}

#[test]
fn validates_ui_values_in_unselected_dictionaries() {
    for invalid in [
        "{nested={{{function() end}}}}",
        "{nested={true,[3]=false}}",
        r"{nested={['\255']=true}}",
        r"{nested={['\239\187\191key']=true}}",
        r"{nested={['key\0']=true}}",
        r"{nested={'\255'}}",
        r"{nested={'\239\187\191value'}}",
        r"{nested={'value\0'}}",
    ] {
        let src = format!(
            "return {{ui_opts={{['org.example.nauvim.test']={{valid=true}},unused={invalid}}}}}"
        );
        let error = match ui_options(&src) {
            Ok(_) => panic!("invalid unselected entry was accepted: {invalid}"),
            Err(e) => e,
        };
        assert!(
            error.to_string().contains("UI option") || error.to_string().contains("ui_opts"),
            "unexpected error for {invalid}: {error}"
        );
    }
    assert!(
        ui_options("return {ui_opts=false}")
            .unwrap()
            .root()
            .entries()
            .len()
            == 0
    );
    assert!(
        ui_options("return {ui_opts={[1]={}}}")
            .unwrap()
            .root()
            .entries()
            .len()
            == 0
    );
}

#[test]
fn validates_ui_mapping_keys_selectors_and_implementation_ids() {
    for value in [
        "''",
        "'\\239\\187\\191leading'",
        "'nul\\0inside'",
        "'\\255'",
    ] {
        let src = format!("return {{use_ui_opts={value}}}");
        assert!(ui_options(&src).is_err(), "selector accepted: {value}");
        let src = format!("return {{ui_opts={{[{value}]={{}}}}}}");
        assert!(ui_options(&src).is_err(), "outer key accepted: {value}");
    }
    for ui_id in ["", "\u{feff}leading", "nul\0inside"] {
        let error = match Config::evaluate(
            "error('must not execute')",
            info(Path::new("/tmp/nauvim-tests/config.lua")),
            ui_id,
            |_| Ok(()),
        ) {
            Ok(_) => panic!("invalid implementation ID accepted: {ui_id:?}"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("UI implementation ID"));
        assert!(Config::load(None, ui_id, |_| Ok(())).is_err());
    }
    for key in [
        "'inner\\239\\187\\191mark'",
        "'trailing\\239\\187\\191'",
        "'日本語'",
    ] {
        let src = format!("return {{ui_opts={{[{key}]={{selected=true}}}},use_ui_opts={key}}}");
        assert_eq!(
            ui_options(&src).unwrap().get("selected"),
            Some(&UiValue::Boolean(true))
        );
    }
    let (_, selected) = Config::evaluate(
        "return {ui_opts={['inner\\239\\187\\191mark']={selected=true}}}",
        info(Path::new("/tmp/nauvim-tests/config.lua")),
        "inner\u{feff}mark",
        Ok,
    )
    .unwrap();
    assert_eq!(selected.get("selected"), Some(&UiValue::Boolean(true)));
}

#[test]
fn recovers_ui_dictionary_entries_before_delivering_the_selected_options() {
    let opts = ui_options(
        r#"local client = require("nauvim-client-info")
        assert(client.ui_id == "org.example.nauvim.test")
        return {
            use_ui_opts = false,
            ui_opts = {
                unused = 42,
                [1] = {},
                [client.ui_id] = {
                    handler = function() end,
                    [2] = "discard",
                    nested = {{ [false] = true, keep = "kept", extra = io.stdout }},
                },
            },
        }"#,
    )
    .unwrap();
    assert_eq!(opts.get("handler"), None);
    let list = opts.table(table_id(opts.get("nested"))).unwrap();
    let nested = opts
        .table(table_id(list.entries().next().map(|(_, v)| v)))
        .unwrap();
    assert_eq!(nested.get("keep"), Some(&UiValue::String("kept".into())));
    assert_eq!(nested.entries().len(), 1);
}

#[test]
fn delivers_nested_ui_lists_and_string_keyed_dictionaries() {
    let opts = ui_options(
        r#"local client = require("nauvim-client-info")
            assert(client.ui_id == "org.example.nauvim.test")
            return {
            ui_opts={['org.example.nauvim.test']={
                ['kebab-key']=-3,
                ['']='',
                ['key\239\187\191end']='日本語\239\187\191',
                ratio=12.5,
                infinity=math.huge,
                fonts={[2]='Fallback', [1]={ family='Mono', size=16 }},
            }},
        }"#,
    )
    .unwrap();
    assert_eq!(opts.get("kebab-key"), Some(&UiValue::Integer(-3)));
    assert_eq!(opts.get(""), Some(&UiValue::String("".into())));
    assert_eq!(
        opts.get("key\u{feff}end"),
        Some(&UiValue::String("日本語\u{feff}".into()))
    );
    assert_eq!(opts.get("ratio"), Some(&UiValue::Number(12.5)));
    assert_eq!(opts.get("infinity"), Some(&UiValue::Number(f64::INFINITY)));
    assert!(!opts.root().is_list());
    let fonts = opts.table(table_id(opts.get("fonts"))).unwrap();
    assert!(fonts.is_list());
    let entries = fonts.entries().collect::<Vec<_>>();
    assert_eq!(entries[0].0, &UiValue::Integer(1));
    assert_eq!(
        entries[1],
        (&UiValue::Integer(2), &UiValue::String("Fallback".into()))
    );
    let font = opts.table(table_id(Some(entries[0].1))).unwrap();
    assert!(!font.is_list());
    assert_eq!(font.get("family"), Some(&UiValue::String("Mono".into())));
    assert_eq!(font.get("size"), Some(&UiValue::Integer(16)));
}

fn table_id(value: Option<&UiValue>) -> UiTableId {
    let Some(UiValue::Table(id)) = value else {
        panic!("expected a table reference, received {value:?}")
    };
    *id
}

#[test]
fn keeps_shared_ui_tables_and_rejects_cycles() {
    let opts = ui_options(
        r#"
        local child={value=42}
        local shared={child=child}
        local options={first=shared,second=shared,third=child}
        return {ui_opts={['org.example.nauvim.test']=options,unused={shared=shared}}}
        "#,
    )
    .unwrap();
    assert_eq!(opts.get("first"), opts.get("second"));
    let shared = opts.table(table_id(opts.get("first"))).unwrap();
    assert_eq!(shared.get("child"), opts.get("third"));
    let child = opts.table(table_id(shared.get("child"))).unwrap();
    assert_eq!(child.get("value"), Some(&UiValue::Integer(42)));

    for src in [
        "local t={}; t.self=t; return {ui_opts={unused=t}}",
        "local a,b={},{}; a.b=b; b.a=a; return {ui_opts={unused={a=a,b=b}}}",
    ] {
        assert!(ui_options(src).is_err(), "{src}");
    }
}

fn parse_size(opts: UiConfig) -> Result<i64, Error> {
    match opts.get("size") {
        None => Ok(12),
        Some(UiValue::Integer(size)) => Ok(*size),
        _ => Err(Error::Message("UI size must be an integer".into())),
    }
}

#[test]
fn interprets_only_selected_ui_schema_after_all_core_validation() {
    let src = r#"return {
        ui_opts={
            ['org.example.nauvim.test']={size=16},
            other={size='another frontend representation'},
        },
    }"#;
    let (_, size) = Config::evaluate(
        src,
        info(Path::new("/tmp/nauvim-tests/config.lua")),
        UI_ID,
        parse_size,
    )
    .unwrap();
    assert_eq!(size, 16);
    let error = match Config::evaluate(
        src,
        info(Path::new("/tmp/nauvim-tests/config.lua")),
        "other",
        parse_size,
    ) {
        Ok(_) => panic!("invalid selected schema accepted"),
        Err(e) => e,
    };
    assert!(matches!(error, Error::UiOptions(_)));
    for invalid in [
        "return {ssh={config='relative'},ui_opts={}}",
        "return {ui_opts={unused={f={function() end}}}}",
    ] {
        let mut called = false;
        let result = Config::evaluate(
            invalid,
            info(Path::new("/tmp/nauvim-tests/config.lua")),
            UI_ID,
            |_| {
                called = true;
                Ok(())
            },
        );
        assert!(result.is_err(), "{invalid}");
        assert!(
            !called,
            "schema parser ran before core validation: {invalid}"
        );
    }
}

#[test]
fn replaces_entire_configuration_after_selected_ui_schema_failure() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("init.lua");
    let src = r#"return {
        hosts={custom={}},default_host='custom',
        clipboard={backend='xclip',timeout_ms=9},
        ssh={config='/custom/config',known_hosts='/custom/known_hosts'},
        ui_opts={['org.example.nauvim.test']={size='invalid'}},
    }"#;
    fs::write(&file, src).unwrap();
    let mut calls = 0;
    let (config, size) = Config::load(Some(&file), UI_ID, |opts| {
        calls += 1;
        parse_size(opts)
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(size, 12);
    assert_eq!(config.startup_host(), Some(HostSelection::Default));
    assert!(config.hosts().next().is_none());
    assert_eq!(config.default_host(), None);
    assert!(config.client_info().is_none());
    assert_eq!(config.clipboard().backend(), "auto");
    assert_eq!(config.clipboard().timeout().as_millis(), 3000);
    assert_eq!(config.diagnostics().len(), 1);
    assert!(config.diagnostics()[0].contains("Parse UI options"));
}

#[test]
fn passes_only_empty_defaults_to_parser_after_common_validation_failure() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("init.lua");
    fs::write(
        &file,
        "return {hosts={custom={}},ui_opts={unused={f={function() end}}}}",
    )
    .unwrap();
    let mut calls = 0;
    let (config, ()) = Config::load(Some(&file), UI_ID, |opts| {
        calls += 1;
        assert_eq!(opts.root().entries().len(), 0);
        Ok(())
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(config.startup_host(), Some(HostSelection::Default));
    assert!(config.hosts().next().is_none());
    assert_eq!(config.diagnostics().len(), 1);
    assert!(config.diagnostics()[0].contains("Unsupported UI option list element type: function"));
}

#[test]
fn reports_frontend_rejection_of_empty_default_options() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing.lua");
    let error = match Config::load(Some(&missing), UI_ID, |opts| {
        assert_eq!(opts.root().entries().len(), 0);
        Err::<(), _>(Error::Message("frontend default failure".into()))
    }) {
        Ok(_) => panic!("invalid frontend defaults accepted"),
        Err(e) => e,
    };
    assert!(matches!(error, Error::DefaultUiOptions { .. }));
    assert!(error.to_string().contains("Parse default UI options"));
    assert!(error.to_string().contains("frontend default failure"));
}

#[test]
fn owns_selected_ui_options_independently_of_core_configuration() {
    let (config, opts) = Config::evaluate(
        "return {ui_opts={['org.example.nauvim.test']={nested={value='retained'}}}}",
        info(Path::new("/tmp/nauvim-tests/config.lua")),
        UI_ID,
        Ok,
    )
    .unwrap();
    drop(config);
    let nested = opts.table(table_id(opts.get("nested"))).unwrap();
    assert_eq!(
        nested.get("value"),
        Some(&UiValue::String("retained".into()))
    );
}
