use super::*;
use nvim_rpc_plugin::mp::decode::{Object as Decoded, ObjectParser};
use nvim_rpc_plugin::mp::encode::{Primitive, WriterSyncExt as _};

use tempfile::TempDir;

fn dispatch(client_id: u32) -> (Dispatch, mpsc::UnboundedReceiver<EngineEvent>, TempDir) {
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let (events, receiver) = mpsc::unbounded_channel();
    #[cfg(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http"
    ))]
    let resources = {
        let (errors, _) = mpsc::unbounded_channel();
        Arc::new(Resources::new(dir.path().to_owned(), errors))
    };
    (
        Dispatch {
            client_id: client_id.into(),
            host_quit: Arc::new(AtomicBool::new(false)),
            host_detach: Arc::new(AtomicBool::new(false)),
            #[cfg(any(
                feature = "plugin-local-forward",
                feature = "plugin-filesystem",
                feature = "plugin-http"
            ))]
            resources,
            events,
            #[cfg(feature = "plugin-http")]
            grids: Grids::default(),
            #[cfg(any(
                feature = "plugin-local-forward",
                feature = "plugin-filesystem",
                feature = "plugin-http",
                feature = "plugin-clipboard"
            ))]
            policies: Policies {
                local_forward: false,
                filesystem: false,
                http: false,
                clipboard: false,
            },
            #[cfg(feature = "plugin-clipboard")]
            clipboard: Arc::default(),
            #[cfg(feature = "plugin-clipboard")]
            clipboard_errors: mpsc::unbounded_channel().0,
        },
        receiver,
        dir,
    )
}

fn params(bytes: &[u8]) -> Array {
    let mut parser = ObjectParser::new(Bytes::copy_from_slice(bytes));
    let Decoded::Array(params) = parser.next_object().unwrap() else {
        panic!("Test input must be an array");
    };
    assert!(parser.remaining().is_empty());
    params
}

fn encoded(reply: ReplyValue) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.write_value(reply).consume();
    bytes
}

fn arguments(value: impl Primitive) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.write_primitive(value).consume();
    bytes
}

#[tokio::test]
async fn ignores_invalid_client_ids_before_method_policy_and_argument_validation() {
    use std::fs::read_dir;

    let (dispatch, mut events, dir) = dispatch(7);
    let inputs: &[&[u8]] = &[
        b"\x90",                                         // Missing ID.
        b"\x92\xc0\xc3",                                 // Nil ID and invalid operation arguments.
        b"\x92\xc3\xc3",                                 // Boolean ID.
        b"\x92\xa17\xc3",                                // String ID.
        b"\x92\xff\xc3",                                 // Negative ID.
        b"\x92\xcf\x00\x00\x00\x01\x00\x00\x00\x00\xc3", // u32::MAX + 1.
        b"\x92\x06\xc3",                                 // A different process ID.
        b"\x92\xcb\x40\x1c\x00\x00\x00\x00\x00\x00\xc3", // Float 7.0.
        b"\x92\x90\xc3",                                 // Array ID.
    ];
    for input in inputs {
        for method in [
            "nauvim-plugin/local-forward.open_tcp",
            "nauvim-plugin/local-forward.open_socket",
            "nauvim-plugin/local-forward.get_port",
            "nauvim-plugin/filesystem.copy",
            "nauvim-plugin/http.serve_file",
            "nauvim-plugin/http.get_port",
            "nauvim-plugin/http.open_webview",
            "nauvim-plugin/clipboard.copy",
            "nauvim-plugin/clipboard.paste",
            "nauvim-plugin/client.host_quit",
            "nauvim-plugin/client.host_detach",
            "nauvim-plugin/unknown",
        ] {
            let reply = dispatch
                .request(method.as_bytes(), params(input))
                .await
                .unwrap();
            assert_eq!(encoded(reply), b"\xc0", "method={method}, input={input:?}");
        }
        for method in [
            "nauvim-plugin/client.connect",
            "nauvim-plugin/clipboard.copy",
            "nauvim-plugin/local-forward.clear",
            "nauvim-plugin/filesystem.clear",
            "nauvim-plugin/http.clear",
            "nauvim-plugin/unknown",
        ] {
            assert!(
                dispatch
                    .notification(method.as_bytes(), params(input))
                    .await
                    .is_ok(),
                "method={method}, input={input:?}"
            );
        }
    }
    assert!(events.try_recv().is_err());
    assert!(!dispatch.host_quit.load(Ordering::Acquire));
    assert!(!dispatch.host_detach.load(Ordering::Acquire));
    assert_eq!(read_dir(dir.path()).unwrap().count(), 0);
    #[cfg(any(
        feature = "plugin-local-forward",
        feature = "plugin-filesystem",
        feature = "plugin-http"
    ))]
    {
        // No invalid-ID call reserved a resource, even when its method was
        // recognized and resource permissions were disabled.
        assert_eq!(dispatch.resources.reserve().await.unwrap().get(), 1);
        dispatch.resources.cleanup().await;
    }
}

#[tokio::test]
#[cfg(feature = "plugin-clipboard")]
async fn reports_copy_notification_backend_errors_without_an_rpc_response() {
    use tokio::time::{Duration, timeout};

    let (mut dispatch, events, _dir) = dispatch(7);
    drop(events);
    let (errors, mut reports) = mpsc::unbounded_channel();
    dispatch.clipboard_errors = errors;
    unavailable_clipboard(&mut dispatch);
    dispatch.policies.clipboard = true;
    for (register, text) in [("+", ""), ("*", "日本語🙂\nsecond"), ("+", "line\n")] {
        for input in [
            arguments((7, register, text)),
            arguments((7, register.as_bytes(), text.as_bytes())),
        ] {
            dispatch
                .notification(b"nauvim-plugin/clipboard.copy", params(&input))
                .await
                .unwrap();
            let error = timeout(Duration::from_secs(1), reports.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(error.contains("missing-test-backend"));
        }
    }
    assert!(reports.try_recv().is_err());
}

#[cfg(feature = "plugin-clipboard")]
fn unavailable_clipboard(dispatch: &mut Dispatch) {
    use crate::ClipboardConfig;

    use mlua::{Lua, Value};

    let lua = Lua::new();
    let config = ClipboardConfig::parse(
        Value::String(lua.create_string("missing-test-backend").unwrap()),
        &mut Vec::new(),
    )
    .unwrap();
    let (clipboard, _) = Clipboard::new(&config);
    dispatch.clipboard = Arc::new(clipboard);
}

#[tokio::test]
#[cfg(feature = "plugin-clipboard")]
async fn validates_copy_notification_arguments_before_backend_access() {
    let (mut dispatch, mut events, _dir) = dispatch(7);
    unavailable_clipboard(&mut dispatch);
    dispatch.policies.clipboard = true;
    let invalid = [
        (arguments((7,)), "Missing RPC argument"),
        (arguments((7, "+")), "Missing RPC argument"),
        (arguments((7, Nil, "text")), "expected string"),
        (arguments((7, true, "text")), "expected string"),
        (arguments((7, 1, "text")), "expected string"),
        (arguments((7, ("+",), "text")), "expected string"),
        (
            arguments((7, "", "text")),
            "Clipboard register must be + or *",
        ),
        (
            arguments((7, "clipboard", "text")),
            "Clipboard register must be + or *",
        ),
        (
            arguments((7, "++", "text")),
            "Clipboard register must be + or *",
        ),
        (arguments((7, "+", Nil)), "expected string"),
        (arguments((7, "+", false)), "expected string"),
        (arguments((7, "+", 1)), "expected string"),
        (arguments((7, "+", ("line",))), "expected string"),
        (arguments((7, "+", "text", true)), "Too many RPC arguments"),
        (
            arguments((7, "+", "a\0b")),
            "RPC strings must not begin with U+FEFF or contain NUL",
        ),
        (
            arguments((7, "+", "\u{feff}b")),
            "RPC strings must not begin with U+FEFF or contain NUL",
        ),
        (
            arguments((7, "+", &b"a\0b"[..])),
            "RPC strings must not begin with U+FEFF or contain NUL",
        ),
        (
            arguments((7, "+", &b"\xef\xbb\xbfb"[..])),
            "RPC strings must not begin with U+FEFF or contain NUL",
        ),
        (
            arguments((7, "+", &b"\xff"[..])),
            "RPC strings must be valid UTF-8",
        ),
    ];
    for (input, expected) in invalid {
        let error = dispatch
            .notification(b"nauvim-plugin/clipboard.copy", params(&input))
            .await
            .unwrap_err();
        assert_eq!(error, expected, "input={input:?}");
    }
    assert!(events.try_recv().is_err());
}

#[tokio::test]
#[cfg(feature = "plugin-clipboard")]
async fn validates_copy_client_id_and_policy_before_backend_access() {
    let (mut dispatch, mut events, _dir) = dispatch(7);
    unavailable_clipboard(&mut dispatch);
    for input in [
        arguments(()),
        arguments((Nil, "+", "text")),
        arguments((true, "+", "text")),
        arguments(("7", "+", "text")),
        arguments((7.0, "+", "text")),
        arguments((-1, "+", "text")),
        arguments((4_294_967_296_u64, "+", "text")),
        arguments((6, "+", "bad\0text")),
        arguments((8, "+", "text", false)),
    ] {
        dispatch
            .notification(b"nauvim-plugin/clipboard.copy", params(&input))
            .await
            .unwrap();
    }
    let error = dispatch
        .notification(
            b"nauvim-plugin/clipboard.copy",
            params(&arguments((7, "+", "text"))),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "Client policy denies this operation");
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn treats_legacy_clipboard_requests_as_unknown_methods() {
    let (dispatch, mut events, _dir) = dispatch(7);
    for (method, input) in [
        ("nauvim-plugin/clipboard.copy", arguments((7, "+", "text"))),
        (
            "nauvim-plugin/clipboard.copy",
            arguments((7, "+", ("line",), "v")),
        ),
        ("nauvim-plugin/clipboard.paste", arguments((7, "+"))),
    ] {
        let error = dispatch
            .request(method.as_bytes(), params(&input))
            .await
            .err()
            .unwrap();
        assert_eq!(error, "Unknown or disabled Nauvim request", "{method}");
    }
    assert!(events.try_recv().is_err());
}

#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
#[tokio::test]
async fn rejects_invalid_operation_types_and_ranges_before_allocating_resources() {
    use std::fs::read_dir;

    let (mut dispatch, mut events, dir) = dispatch(7);
    dispatch.policies = Policies::default();
    let cases = [
        #[cfg(feature = "plugin-local-forward")]
        (
            "local-forward.open_tcp",
            vec![
                arguments((7, -1, "localhost", 80)),
                arguments((7, 65536, "localhost", 80)),
                arguments((7, 0.0, "localhost", 80)),
                arguments((7, false, "localhost", 80)),
                arguments((7, "0", "localhost", 80)),
                arguments((7, 0, Nil, 80)),
                arguments((7, 0, "localhost", 0)),
                arguments((7, 0, "localhost", 65536)),
                arguments((7, 0, "localhost", 80.0)),
                arguments((7, 0, "localhost", 80, true)),
            ],
        ),
        #[cfg(feature = "plugin-local-forward")]
        (
            "local-forward.open_socket",
            vec![
                arguments((7, true, "host.sock")),
                arguments((7, "client.sock", Nil)),
                arguments((7, "client.sock", "host.sock", false)),
            ],
        ),
        #[cfg(feature = "plugin-filesystem")]
        (
            "filesystem.copy",
            vec![
                arguments((7, false, "host", true)),
                arguments((7, "client", Nil, true)),
                arguments((7, "client", "host", "true")),
                arguments((7, "client", "host", 1)),
                arguments((7, "client", "host", Nil)),
                arguments((7, "client", "host", true, false)),
            ],
        ),
        #[cfg(feature = "plugin-http")]
        (
            "http.serve_file",
            vec![
                arguments((7, "file", -1, true, true)),
                arguments((7, "file", 65536, true, true)),
                arguments((7, "file", 0.0, true, true)),
                arguments((7, "file", 0, "true", true)),
                arguments((7, "file", 0, 1, true)),
                arguments((7, "file", 0, true, Nil)),
                arguments((7, "file", 0, false, false)),
                arguments((7, "file", 0, true, true, false)),
            ],
        ),
        #[cfg(feature = "plugin-local-forward")]
        (
            "local-forward.get_port",
            vec![
                arguments((7, 1.0)),
                arguments((7, "1")),
                arguments((7, true)),
                arguments((7, Nil)),
                arguments((7, u64::MAX)),
                arguments((7, 1, true)),
            ],
        ),
        #[cfg(feature = "plugin-http")]
        (
            "http.get_port",
            vec![
                arguments((7, 1.0)),
                arguments((7, "1")),
                arguments((7, false)),
                arguments((7, Nil)),
                arguments((7, u64::MAX)),
                arguments((7, 1, false)),
            ],
        ),
    ];
    for (method, inputs) in cases {
        let method = format!("nauvim-plugin/{method}");
        for input in inputs {
            let error = dispatch
                .request(method.as_bytes(), params(&input))
                .await
                .err()
                .unwrap();
            assert!(
                error.starts_with("expected ")
                    || error.contains("out of range")
                    || error.contains("Too many")
                    || error.contains("Host TCP port")
                    || error.contains("At least one"),
                "method={method}, input={input:?}, error={error}",
            );
        }
    }
    assert_eq!(read_dir(dir.path()).unwrap().count(), 0);
    assert_eq!(dispatch.resources.reserve().await.unwrap().get(), 1);
    assert!(events.try_recv().is_err());
    dispatch.resources.cleanup().await;
}

#[tokio::test]
async fn validates_notification_host_and_handle_types_before_operations() {
    let (dispatch, mut events, _dir) = dispatch(7);
    for input in [
        arguments((7, Nil)),
        arguments((7, true)),
        arguments((7, 1)),
        arguments((7, ("host",))),
        arguments((7, "host", false)),
    ] {
        let error = dispatch
            .notification(b"nauvim-plugin/client.connect", params(&input))
            .await
            .unwrap_err();
        assert!(
            error == "expected string" || error == "Too many RPC arguments",
            "{error}"
        );
    }
    let methods: &[&str] = &[
        #[cfg(feature = "plugin-local-forward")]
        "nauvim-plugin/local-forward.clear",
        #[cfg(feature = "plugin-filesystem")]
        "nauvim-plugin/filesystem.clear",
        #[cfg(feature = "plugin-http")]
        "nauvim-plugin/http.clear",
    ];
    for method in methods {
        for input in [
            arguments((7, Nil)),
            arguments((7, true)),
            arguments((7, "1")),
            arguments((7, 1.0)),
            arguments((7, u64::MAX)),
            arguments((7, 1, false)),
        ] {
            let error = dispatch
                .notification(method.as_bytes(), params(&input))
                .await
                .unwrap_err();
            assert!(
                error == "expected integer"
                    || error == "integer is out of range"
                    || error == "Too many RPC arguments",
                "method={method}, error={error}"
            );
        }
    }
    assert!(events.try_recv().is_err());
}

#[cfg(feature = "plugin-http")]
#[tokio::test]
async fn validates_webview_dictionary_fields_before_allocating_a_handle() {
    use nvim_rpc_plugin::mp::encode::primitive::Map;

    fn position(field: &str, value: impl Primitive) -> Vec<u8> {
        let mut entries: Vec<_> = [
            ("grid", 1_i64),
            ("top", 0),
            ("left", 0),
            ("height", 1),
            ("width", 1),
        ]
        .into_iter()
        .filter(|(key, _)| *key != field)
        .map(|(key, value)| (key, Untagged::Left(value)))
        .collect();
        entries.push((field, Untagged::Right(value)));
        arguments((7, "https://example.test/path#fragment", Map(entries)))
    }

    let (mut dispatch, mut events, _dir) = dispatch(7);
    dispatch.policies.http = true;
    for field in ["top", "left", "height", "width"] {
        for input in [
            position(field, -1),
            position(field, 65536),
            position(field, 1.0),
            position(field, true),
            position(field, "1"),
            position(field, Nil),
        ] {
            let error = dispatch
                .request(b"nauvim-plugin/http.open_webview", params(&input))
                .await
                .err()
                .unwrap();
            assert!(
                error == "expected integer" || error == "integer is out of range",
                "field={field}, error={error}"
            );
        }
    }
    for field in ["grid", "top", "left", "height", "width"] {
        let mut entries = vec![
            ("grid", 1),
            ("top", 0),
            ("left", 0),
            ("height", 1),
            ("width", 1),
        ];
        entries.push((field, 1));
        let input = arguments((7, "https://example.test", Map(entries)));
        let error = dispatch
            .request(b"nauvim-plugin/http.open_webview", params(&input))
            .await
            .err()
            .unwrap();
        assert_eq!(error, format!("Duplicate position.{field}"));
    }
    for input in [
        arguments((7, "https://example.test", (1, 0, 0, 1, 1))),
        arguments((7, "https://example.test", Map([("grid", 1)]))),
        arguments((7, "https://example.test", Map([(1, 0)]))),
        arguments((7, "file:///etc/passwd", Map([("grid", 1)]))),
        position("unknown", 1),
    ] {
        assert!(
            dispatch
                .request(b"nauvim-plugin/http.open_webview", params(&input))
                .await
                .is_err(),
            "input={input:?}"
        );
    }
    assert_eq!(dispatch.resources.reserve().await.unwrap().get(), 1);
    assert!(events.try_recv().is_err());

    let input = arguments((
        7,
        "https://example.test/path#fragment",
        Map([
            ("grid", 999),
            ("top", 0),
            ("left", 65535),
            ("height", 0),
            ("width", 65535),
        ]),
    ));
    assert_eq!(
        encoded(
            dispatch
                .request(b"nauvim-plugin/http.open_webview", params(&input))
                .await
                .unwrap()
        ),
        b"\x02"
    );
    assert!(
        events.try_recv().is_err(),
        "A missing grid must not create a window"
    );
    dispatch.resources.cleanup().await;
}

#[tokio::test]
async fn accepts_both_client_id_boundaries_and_validates_matching_calls() {
    for (client_id, input) in [
        (0, &b"\x91\x00"[..]),
        (0, &b"\x91\xd0\x00"[..]),
        (u32::MAX, &b"\x91\xce\xff\xff\xff\xff"[..]),
        (u32::MAX, &b"\x91\xd3\x00\x00\x00\x00\xff\xff\xff\xff"[..]),
    ] {
        let (dispatch, mut events, _dir) = dispatch(client_id);
        assert_eq!(
            dispatch
                .request(b"nauvim-plugin/unknown", params(input))
                .await
                .err()
                .unwrap(),
            "Unknown or disabled Nauvim request"
        );
        assert_eq!(
            dispatch
                .notification(b"nauvim-plugin/client.connect", params(input))
                .await
                .unwrap_err(),
            "Missing RPC argument"
        );
        assert!(events.try_recv().is_err());
    }
    let (dispatch, mut events, _dir) = dispatch(7);
    dispatch
        .notification(b"nauvim-plugin/client.connect", params(b"\x92\x07\xa4next"))
        .await
        .unwrap();
    let EngineEvent::Connect { host } = events.recv().await.unwrap() else {
        panic!("Expected a connection event");
    };
    assert_eq!(host, "next");
    assert!(
        dispatch
            .notification(b"nauvim-plugin/client.connect", params(b"\x92\x07\xc0"))
            .await
            .is_err()
    );
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn marks_host_lifecycle_only_after_matching_client_and_arity_checks() {
    let (dispatch, mut events, _dir) = dispatch(7);
    for (method, flag) in [
        (&b"nauvim-plugin/client.host_quit"[..], &dispatch.host_quit),
        (
            &b"nauvim-plugin/client.host_detach"[..],
            &dispatch.host_detach,
        ),
    ] {
        assert_eq!(
            encoded(dispatch.request(method, params(b"\x91\x06")).await.unwrap()),
            b"\xc0"
        );
        assert!(!flag.load(Ordering::Acquire));
        assert!(
            dispatch
                .request(method, params(b"\x92\x07\xc3"))
                .await
                .is_err()
        );
        assert!(!flag.load(Ordering::Acquire));
        assert_eq!(
            encoded(dispatch.request(method, params(b"\x91\x07")).await.unwrap()),
            b"\xc0"
        );
        assert!(flag.load(Ordering::Acquire));
    }
    assert!(events.try_recv().is_err());
}

#[cfg(any(
    feature = "plugin-local-forward",
    feature = "plugin-filesystem",
    feature = "plugin-http"
))]
#[tokio::test]
async fn validates_arguments_after_a_matching_client_id() {
    let (mut dispatch, mut events, _dir) = dispatch(7);
    dispatch.policies = Policies::default();
    for method in [
        "nauvim-plugin/local-forward.open_tcp",
        "nauvim-plugin/local-forward.open_socket",
        "nauvim-plugin/local-forward.get_port",
        "nauvim-plugin/filesystem.copy",
        "nauvim-plugin/http.serve_file",
        "nauvim-plugin/http.get_port",
        "nauvim-plugin/http.open_webview",
    ] {
        assert!(
            dispatch
                .request(method.as_bytes(), params(b"\x91\x07"))
                .await
                .is_err(),
            "{method}"
        );
    }
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn rejects_invalid_strings_at_every_nesting_level_after_client_id() {
    let (dispatch, mut events, _dir) = dispatch(7);
    for input in [
        &b"\x92\x07\xa1\0"[..],
        &b"\x92\x07\xa3\xef\xbb\xbf"[..],
        &b"\x92\x07\xa1\xff"[..],
        &b"\x92\x07\xc4\x01\0"[..],
        &b"\x92\x07\xc4\x03\xef\xbb\xbf"[..],
        &b"\x92\x07\xc4\x01\xff"[..],
        &b"\x92\x07\x91\xa2x\0"[..],
        &b"\x92\x07\x91\xa3\xef\xbb\xbf"[..],
        &b"\x92\x07\x91\xc4\x02x\0"[..],
        &b"\x92\x07\x81\xa3\xef\xbb\xbf\xc0"[..],
        &b"\x92\x07\x81\xc4\x03\xef\xbb\xbf\xc0"[..],
        &b"\x92\x07\x81\xa1x\xa1\0"[..],
        &b"\x92\x07\x81\xa1x\xc4\x01\0"[..],
        &b"\x92\x07\x81\xa1x\xa3\xef\xbb\xbf"[..],
        &b"\x92\x07\x81\xa1x\x91\xa1\xff"[..],
    ] {
        let error = dispatch
            .request(b"nauvim-plugin/unknown", params(input))
            .await
            .err()
            .unwrap();
        assert!(error.contains("RPC strings"), "{input:?}: {error}");
        let mut wrong_id = input.to_vec();
        wrong_id[1] = 6;
        assert_eq!(
            encoded(
                dispatch
                    .request(b"nauvim-plugin/unknown", params(&wrong_id))
                    .await
                    .unwrap()
            ),
            b"\xc0"
        );
    }
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn permits_interior_and_trailing_feff_at_every_nesting_level() {
    let (dispatch, mut events, _dir) = dispatch(7);
    for input in [
        &b"\x92\x07\xa5x\xef\xbb\xbfy"[..],
        &b"\x92\x07\xa4x\xef\xbb\xbf"[..],
        &b"\x92\x07\xc4\x04x\xef\xbb\xbf"[..],
        &b"\x92\x07\x91\xa4x\xef\xbb\xbf"[..],
        &b"\x92\x07\x91\xc4\x04x\xef\xbb\xbf"[..],
        &b"\x92\x07\x81\xa4x\xef\xbb\xbf\xc0"[..],
        &b"\x92\x07\x81\xc4\x04x\xef\xbb\xbf\xc0"[..],
        &b"\x92\x07\x81\xa1x\x81\xa1y\xa4z\xef\xbb\xbf"[..],
    ] {
        let error = dispatch
            .request(b"nauvim-plugin/unknown", params(input))
            .await
            .err()
            .unwrap();
        assert_eq!(error, "Unknown or disabled Nauvim request", "{input:?}");
        dispatch
            .notification(b"nauvim-plugin/unknown", params(input))
            .await
            .unwrap();
    }
    assert!(events.try_recv().is_err());
}

#[test]
fn keeps_wire_errors_nonempty_and_free_of_forbidden_characters() {
    assert_eq!(wire_error(String::new()), "Nauvim request failed");
    assert_eq!(wire_error("a\0b\u{feff}".into()), "a\u{fffd}b\u{feff}");
    assert_eq!(wire_error("ordinary failure".into()), "ordinary failure");
    assert_eq!(wire_error("\u{feff}failure".into()), "\u{fffd}failure");
    assert_eq!(
        wire_error("\u{feff}\u{feff}\0".into()),
        "\u{fffd}\u{feff}\u{fffd}"
    );
    assert_eq!(
        wire_error("failure\u{feff}text".into()),
        "failure\u{feff}text"
    );
    assert_eq!(wire_error("failure\u{feff}".into()), "failure\u{feff}");
}
