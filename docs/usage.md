# Configuration reference

`nauvim-core` evaluates configuration in the client environment. This reference
describes its Lua settings and defaults. For toolkit-independent application
integration, see the [frontend implementor guide](implementors.md).

## Loading configuration

The frontend passes an optional path, its UI implementation ID, and its UI-option
parser to `Config::load()`. The default path is
`$XDG_CONFIG_HOME/nauvim/init.lua`, using the platform configuration directory
when `XDG_CONFIG_HOME` is unset. The engine evaluates the file using vendored
Lua 5.4 in the client environment.

Before execution, the engine acquires complete client `HostInfo` and installs
`require("nauvim-client-info")`. Its path fields are absolute with symbolic links
resolved. Its non-nil `config_file` identifies the selected configuration file.
The engine prepends `ROOT/lua/?.lua;ROOT/lua/?/init.lua` to `package.path`, where
`ROOT` is that resolved file's parent directory. Existing entries follow these
templates. Neovim runtime directories are not added.

A read, execution, information-acquisition, or validation failure selects the
complete default configuration, including an empty UI-option dictionary. This
includes failures from the frontend's selected-option parser.
`Config::diagnostics()` retains the error; `Engine::next_event()` emits it as
`EngineEvent::Error`. `Config::evaluate()` is also available for supplied source
and client information. It returns evaluation and validation errors directly
rather than applying the file-loading fallback. See
[UI-specific configuration](#ui-specific-configuration) for selection and
[frontend integration](implementors.md#ui-specific-configuration) for the parser
interface.

The file returns a configuration table. Predefined field names use snake_case,
except within UI-specific option dictionaries. Host aliases are case-sensitive
strings and can contain punctuation. String settings, including values returned
by configuration functions, must be valid UTF-8 without NUL or an initial
U+FEFF.

```lua
local client = require("nauvim-client-info")

return {
    default_host = "work",
    hosts = {
        laptop = {}, -- No SSH settings: a local host.
        work = {
            ssh = {
                host_name = "work.example.com",
                identity_file = client.home_dir .. "/.ssh/work",
            },
        },
    },
    clipboard = { backend = "auto" },
}
```

The `work` host also reads the selected OpenSSH configuration source for settings
absent from its native SSH table. The default source is `$HOME/.ssh/config`.
Required source errors fail the connection attempt.

## Host plugin permissions

Set permissions for each host under `hosts[alias].policies.plugin`. Omitted, nil,
and empty policies grant no permissions. This includes the default local host
and destinations resolved only through OpenSSH. Configuration loading validates
the permissions for every configured host.

```lua
policies = {
    plugin = {
        filesystem = { copy = true },
        http = {
            serve_file = true,
            open_webview = {
                origins = { "https://docs.example.com" },
                served_files = true,
            },
        },
        local_forward = {
            tcp = { { client_port = 0, host_addr = "localhost", host_port = 8080 } },
            socket = {
                { client_socket_path = "preview.sock", host_socket_path = "/run/preview.sock" },
            },
        },
        clipboard = { copy = true },
    },
}
```

- `filesystem.copy` permits copies into the process temporary directory.
  `http.serve_file` permits serving files from that directory.
- `http.open_webview.origins` permits an explicit list of HTTP(S) origins or
  `"all"`. Write each origin as `scheme://host` or `scheme://host:port`. Matching
  uses the scheme, host, and effective port. With `served_files = true`, the
  Webview can also access active file servers started by the same attachment,
  including servers with ports assigned by the operating system.
- `local_forward.tcp` matches the client port, host address, and host port
  exactly. A client port of `0` requests an operating-system-assigned port.
  `local_forward.socket` matches the resolved client path relative to the
  process temporary directory and the supplied absolute host socket path.
  Either setting can use `"all"` to allow all valid requests of its type.
- `clipboard.copy` permits host-initiated writes to the client clipboard.

Denied requests return an error to the plugin. Denied notifications report an
error in Nauvim. Handle queries, cleanup, Webview focus, and GUI paste follow
their ordinary rules.

## Selecting a startup host

`default_host` is a top-level non-empty string or nil. The default is nil. An
omitted, nil, or empty `hosts` table is an empty host collection.

| Configuration | Startup action |
| --- | --- |
| Empty `hosts`, nil `default_host` | Connect to the default local Host. |
| Non-empty `hosts`, nil `default_host` | Show the welcome page. |
| Non-empty `default_host` | Connect to that alias. |

A configured alias without `ssh` selects a local host. A configured alias with
`ssh`, or an alias absent from `hosts`, uses SSH resolution. SSH destinations
cannot connect when the `ssh` feature is excluded.

The default Host is an independent local choice using the default program and
socket settings. It is separate from every named entry, including entries named
`"local"` or `"default"`. A welcome page should offer that choice together with
`Config::hosts()`. Use `Engine::connect_default()` for it and `Engine::connect()`
for a named alias. `Config::startup_host()` exposes automatic startup selection.

Call `Engine::start()` once with the current UI dimensions to apply this startup
selection. Detachment and recovery return to the welcome page without repeating
automatic startup.

## Programs and sockets

A host's `nvim` value accepts an executable string, a program table, or a function
returning either form. The default is `"nvim"`. In a program table, omitted or nil
`prog`, `args`, and `env` default to `"nvim"`, an empty argument sequence, and an
empty environment mapping.

```lua
nvim = {
    args = { "-O3", "-u", "/host/path/to/init.lua" },
    env = { NVIM_APPNAME = "nauvim" },
}
```

Arguments and environment values are strings. The implementation passes them
literally, with environment entries overriding inherited values. It does not
split an executable string or expand shell expressions within configured values.
Nauvim appends `"--headless", "--listen", socket` immediately after `args`.
A string-valued `nvim` has no configured arguments. Program executability is
checked by launching it, rather than by a separate validation step.

Every new host process receives `NAUVIM_CONNECTION=local` or
`NAUVIM_CONNECTION=ssh`. Core's value overrides inherited and configured
values, including entries in `nvim.env`. A wrapper must pass this value and
the appended arguments through to Neovim.

```lua
nvim = {
    prog = "sh",
    args = { "-c", [[exec "$@"]], "nauvim", "nvim", "-O3" },
}
```

The host's `socket` accepts a string or a function returning a string. Program
and socket strings may be relative and may contain whitespace. A socket value
is one argument, including during SSH execution.

Function-valued settings receive `HostInfo` acquired in the selected host
environment, with `config_file = nil`. Direct values do not acquire host
information for resolving that field. Functions execute in the client Lua
interpreter: `os.getenv()` reads client environment variables, while `host`
describes the target environment.

```lua
nvim = function(host)
    return { prog = host.home_dir .. "/bin/nvim" }
end
```

When `socket` is omitted, Nauvim uses a short hashed filename in a private
`/tmp/nauvim-sockets-<uid>` directory on the machine running Neovim. The
directory uses the host account's effective user ID. The filename depends on
the client hostname, client effective username, and host selection, so
reopening Nauvim finds the same session. To reconnect to a session started with
an older default, set `socket` to that session's path.

An explicit `socket` setting selects the endpoint for discovery. Choose a path
reserved for this client environment. If a local socket path exceeds the
platform limit, the connection error shows the path, its length in bytes, and
the maximum. Set that host's `socket` to a shorter path.

Discovery probes only the selected endpoint. A refused stale socket is removed.
An ambiguous or timed-out check leaves the endpoint in place. Socket checks have
a five-second timeout and attachment has a twenty-second timeout.

### Reattaching to a session

Nauvim keeps a display checkpoint on the client so that a surviving Neovim can
update grids for inactive tabs after reattachment. This applies to local and
SSH hosts. If the host Neovim has restarted, Nauvim starts with a fresh display
model.

Nauvim stores the checkpoints in `ui-checkpoints.sqlite3` in the client data
directory. On Linux, this is `$XDG_DATA_HOME/nauvim` or
`$HOME/.local/share/nauvim` when `XDG_DATA_HOME` is unset. The database can
contain text displayed in the editor. Detachment and client exit preserve it
for later sessions. If the saved checkpoint cannot be read, Nauvim reports the
error and attempts attachment with an empty display model.

## SSH configuration

Global `ssh.config` and `ssh.known_hosts` select the client-side OpenSSH source
and trust file. Their defaults are `$HOME/.ssh/config` and
`$HOME/.ssh/known_hosts`. Omit the global `ssh` table or set it to nil to use
both defaults. Supplied paths must be non-empty absolute paths.

A host's native `ssh` table accepts `host_name`, `user`, `port`, `identity_file`,
`certificate_file`, `identity_agent`, `identities_only`, `proxy_jump`,
`kex_algorithms`, and `host_key_algorithms`. Unknown fields and invalid types
are rejected during configuration loading, including in unused hosts and inline
jump tables. Native host names match exactly; `*`, `?`, and `!` are literal.

Identity, certificate, and agent values are single absolute path strings. Native
paths receive no tilde or environment expansion. No agent is inferred from
`SSH_AUTH_SOCK`. `identities_only` accepts booleans and case-insensitive
`true`, `false`, `yes`, and `no` strings; it does not disable an explicit agent.

Resolution gives precedence to the requested native table. Its `host_name`, or
the requested alias when omitted, supplies a lookup name. One direct lookup of
that name's native SSH table fills absent fields. If fields remain absent, the
selected OpenSSH source supplies further settings. Native defaults apply last.
The native merge is shallow: a supplied false, zero, or empty jump sequence
replaces the corresponding inherited setting. Required OpenSSH-source failures
are returned even when native settings have already supplied other fields.

Jump hosts accept an alias, an inline SSH table, or a sequence containing both.
Inline tables use the same supplementation rules, with `host_name` as the lookup
name. Only the first jump host's nested `proxy_jump` adds ancestors. Definitions
must be acyclic; the engine does not check them for cycles. SSH sessions never
request a pseudo-terminal, regardless of OpenSSH `RequestTTY` settings.

Algorithm values accept ordered comma-separated strings or string arrays.
Replacement lists and the first-entry modifiers `+`, `-`, and `^` replace,
append to, remove from, or prepend to transport defaults. Certificates precede
plain host-key algorithms. An explicit host-key policy disables known-host
reordering for that destination. KEX extension signals are managed internally.

The attachment's RPC stream, forwarding channels, and SFTP operations share one
SSH connection. Remote host startup runs detached from that connection.

## Clipboard

The top-level `clipboard` value accepts a backend string or a table with optional
`backend` and `timeout_ms`. Omission or nil selects `"auto"` and a timeout of
3000 milliseconds. The same defaults apply to omitted table fields. A supplied
non-nil timeout must have Lua integer type and be at least 1. Invalid timeouts,
including `1000.0`, produce a diagnostic and retain the default timeout without
rejecting the rest of the configuration.

Backends are `auto`, `xclip`, `xsel`, `wl_clipboard`, and `pbcopy`. The engine
checks client-side utilities and the graphical environment after configuration
execution. Detection prefers Wayland and then X11 on Linux, and `pbcopy` on
macOS. Detection failure leaves normal editing available and emits a client
error. The selected backend is retained; operation failures also emit errors.
The host-side provider needs no clipboard utility. Attachment cleanup stops
foreground clipboard owners; clipboard persistence afterward depends on the
client desktop environment.

Bind GUI paste actions to `Engine::paste()`. Host-initiated clipboard writes
require the host's `policies.plugin.clipboard.copy` permission. See
[clipboard and GUI paste](implementors.md#clipboard-and-gui-paste) for input,
event-loop, and error-handling contracts.

<a id="fonts"></a>

## UI-specific configuration

`ui_opts` maps selection keys to option dictionaries. Omission or nil means an
empty mapping. `use_ui_opts` is a string or nil and defaults to nil. The frontend
documents its UI implementation ID and its option schema, including font
settings and defaults. Core defines no font schema or font defaults.

For example, assume the frontend documents the ID `org.example.nauvim.gui` and
accepts the following `fonts` setting:

```lua
return {
    ui_opts = {
        ["org.example.nauvim.gui"] = {
            fonts = { { family = "Example Mono", size = 16 } },
        },
        ["shared-settings"] = {},
    },
    -- use_ui_opts = "shared-settings",
}
```

The ID, schema, and values in this example are illustrative. With `use_ui_opts`
omitted, core selects the entry matching the frontend's supplied ID exactly.
Setting `use_ui_opts = "shared-settings"` selects that key instead. An absent or
nil entry yields an empty dictionary. An explicit selector that has no entry
also yields an empty dictionary; it does not fall back to the implementation ID.
Selection never merges dictionaries.

Every outer mapping key, non-nil selector, and supplied implementation ID must
be non-empty valid UTF-8, must not begin with U+FEFF, and must not contain NUL.
Every non-nil entry must be a dictionary with string keys. Values may be nil,
booleans, integers, floating-point numbers, strings, lists, or string-keyed
dictionaries. Nested Lua lists use contiguous integer keys starting at `1`.
Core rejects cyclic tables and values such as functions, threads, and userdata.
All string keys and values must be valid UTF-8 without NUL or an initial U+FEFF.
Inner strings may be empty and may contain U+FEFF after the first character.
Core checks these requirements for all entries, including unselected entries.

Dictionary field names may use any valid string. The frontend validates the
selected dictionary against its own schema. A common or selected-schema failure during
file loading selects the complete default configuration and produces a
diagnostic as described under [loading configuration](#loading-configuration).

## Events and cleanup

The [frontend implementor guide](implementors.md#engine-events-and-session-transitions)
describes event dispatch, rendering, input, plugin windows, recovery, and
[shutdown](implementors.md#errors-detachment-and-shutdown). Keep receiving engine
events while showing the welcome page, and await explicit shutdown before
dropping the engine.

## Optional features

All features below are enabled by default. Use `--no-default-features` and select
only the functionality the application needs.

| Feature | Functionality |
| --- | --- |
| `ssh` | Remote sessions, SSH forwarding channels, and SFTP copies |
| `plugin-local-forward` | Loopback TCP and Unix socket forwarding |
| `plugin-filesystem` | Host-to-client recursive copies and cleanup |
| `plugin-http` | Loopback file servers and WebView requests |
| `plugin-clipboard` | Client system clipboard exchange |

Client registration and session switching are always present. Disabled plugin
methods are handled as unknown methods after the process client-ID check.

File serving supports HTTP/1.1 with and without TLS. HTTP/2 requires HTTPS,
so `https = false, http2 = true` returns a request error before server startup.
HTTPS uses generated self-signed certificates. Linux file confinement checks use
`/proc/self/fd`; `/proc` must be available. macOS uses `F_GETPATH`. The opened
file's resolved location is checked before serving it.
