//! Client-side clipboard exchange through selected platform clipboard utilities.

use crate::ClipboardConfig;

use tokio::io::AsyncWriteExt as _;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use std::{env, fs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Xclip,
    Xsel,
    Wayland,
    Pbcopy,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Xclip => "xclip",
            Self::Xsel => "xsel",
            Self::Wayland => "wl_clipboard",
            Self::Pbcopy => "pbcopy",
        }
    }

    fn programs(self) -> (&'static str, &'static str) {
        match self {
            Self::Xclip => ("xclip", "xclip"),
            Self::Xsel => ("xsel", "xsel"),
            Self::Wayland => ("wl-copy", "wl-paste"),
            Self::Pbcopy => ("pbcopy", "pbpaste"),
        }
    }

    fn retains_owner(self) -> bool {
        self != Self::Pbcopy
    }
}

#[derive(Clone)]
struct Commands {
    backend: Backend,
    copy: PathBuf,
    paste: PathBuf,
}

#[derive(Default)]
struct Environment {
    path: Option<OsString>,
    display: Option<OsString>,
    wayland_display: Option<OsString>,
    runtime_dir: Option<OsString>,
}

impl Environment {
    fn current() -> Self {
        Self {
            path: env::var_os("PATH"),
            display: env::var_os("DISPLAY").filter(|s| !s.is_empty()),
            wayland_display: env::var_os("WAYLAND_DISPLAY").filter(|s| !s.is_empty()),
            runtime_dir: env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()),
        }
    }

    fn validate(&self, backend: Backend) -> Result<Commands, String> {
        match backend {
            Backend::Xclip | Backend::Xsel => {
                let display = self.display.as_deref().ok_or("DISPLAY is not set")?;
                // Local X11 displays have a discoverable Unix socket. Remote
                // displays and XQuartz naming schemes are checked at operation time.
                if let Some(display) = display.to_str()
                    && let Some((host, number)) = display.rsplit_once(':')
                    && (host.is_empty() || host == "unix")
                {
                    let number = number.split('.').next().unwrap_or(number);
                    let number = number
                        .parse::<u32>()
                        .map_err(|_| "DISPLAY has an invalid local display number")?;
                    socket(&PathBuf::from(format!("/tmp/.X11-unix/X{number}")))?;
                }
            }
            Backend::Wayland => {
                let display = self
                    .wayland_display
                    .as_deref()
                    .ok_or("WAYLAND_DISPLAY is not set")?;
                let display = Path::new(display);
                let path = if display.is_absolute() {
                    display.to_path_buf()
                } else {
                    let runtime = self
                        .runtime_dir
                        .as_deref()
                        .ok_or("XDG_RUNTIME_DIR is not set")?;
                    Path::new(runtime).join(display)
                };
                socket(&path)?;
            }
            Backend::Pbcopy if !cfg!(target_os = "macos") => {
                return Err("pbcopy requires macOS".into());
            }
            Backend::Pbcopy => {}
        }
        let (copy, paste) = backend.programs();
        Ok(Commands {
            backend,
            copy: executable(copy, self.path.as_deref())?,
            paste: executable(paste, self.path.as_deref())?,
        })
    }

    fn select(&self, requested: &str) -> Result<Commands, String> {
        let backend = match requested {
            "auto" => {
                if cfg!(target_os = "macos") {
                    return self
                        .validate(Backend::Pbcopy)
                        .map_err(|e| format!("Detect clipboard backend: {e}"));
                }
                let mut errors = Vec::new();
                if self.wayland_display.is_some() {
                    match self.validate(Backend::Wayland) {
                        Ok(commands) => return Ok(commands),
                        Err(e) => errors.push(format!("wl_clipboard: {e}")),
                    }
                }
                if self.display.is_some() {
                    for backend in [Backend::Xclip, Backend::Xsel] {
                        match self.validate(backend) {
                            Ok(commands) => return Ok(commands),
                            Err(e) => errors.push(format!("{}: {e}", backend.name())),
                        }
                    }
                }
                return Err(if errors.is_empty() {
                    "Detect clipboard backend: no X11 or Wayland session is configured".into()
                } else {
                    format!("Detect clipboard backend: {}", errors.join("; "))
                });
            }
            "xclip" => Backend::Xclip,
            "xsel" => Backend::Xsel,
            "wl_clipboard" => Backend::Wayland,
            "pbcopy" => Backend::Pbcopy,
            _ => return Err(format!("Unknown clipboard backend: {requested}")),
        };
        self.validate(backend)
            .map_err(|e| format!("Clipboard backend {} is unavailable: {e}", backend.name()))
    }
}

fn socket(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt as _;
    let metadata = fs::metadata(path)
        .map_err(|e| format!("Inspect session socket {}: {e}", path.display()))?;
    if metadata.file_type().is_socket() {
        Ok(())
    } else {
        Err(format!(
            "Session endpoint is not a socket: {}",
            path.display()
        ))
    }
}

fn executable(name: &str, path: Option<&OsStr>) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(path) = path {
        for dir in env::split_paths(path) {
            let path = dir.join(name);
            if let Ok(metadata) = fs::metadata(&path)
                && metadata.is_file()
                && metadata.permissions().mode() & 0o111 != 0
            {
                return Ok(path);
            }
        }
    }
    Err(format!(
        "Required executable {name} is not available on PATH"
    ))
}

#[derive(Default)]
struct Slot {
    owner: Option<Child>,
    closed: bool,
}

/// Exchanges clipboard text without adding host-side dependencies.
///
/// Selection is performed once during construction. Validation checks executable
/// availability, required session variables, and local session socket metadata;
/// server authentication and protocol support are checked by actual operations.
/// Initialization failure leaves editing available and makes clipboard calls fail.
/// On X11 and Wayland, foreground utilities own copied selections until replaced
/// or [`Self::shutdown()`]. Selection owners are independent for `+` and `*`.
pub struct Clipboard {
    commands: Result<Commands, String>,
    timeout: Duration,
    clipboard: Mutex<Slot>,
    primary: Mutex<Slot>,
    tasks: TaskTracker,
    stop: CancellationToken,
    queue: StdMutex<Option<oneshot::Receiver<()>>>,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new(&ClipboardConfig::default()).0
    }
}

impl Clipboard {
    /// Selects a backend and returns client diagnostics for unavailable facilities.
    ///
    /// Does not read or modify clipboard contents. Dependency checks run in the
    /// client environment after configuration evaluation, without spawning tasks.
    pub fn new(config: &ClipboardConfig) -> (Self, Vec<String>) {
        let commands = Environment::current().select(config.backend());
        let diagnostics = commands.as_ref().err().cloned().into_iter().collect();
        (
            Self {
                commands,
                timeout: config.timeout(),
                clipboard: Mutex::default(),
                primary: Mutex::default(),
                tasks: TaskTracker::new(),
                stop: CancellationToken::new(),
                queue: StdMutex::new(None),
            },
            diagnostics,
        )
    }

    pub(crate) fn fork(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            timeout: self.timeout,
            clipboard: Mutex::default(),
            primary: Mutex::default(),
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
            queue: StdMutex::new(None),
        }
    }

    /// Returns the selected backend, or `None` after availability validation failed.
    pub fn backend(&self) -> Option<&'static str> {
        self.commands.as_ref().ok().map(|c| c.backend.name())
    }

    fn slot(&self, register: &str) -> Result<&Mutex<Slot>, String> {
        match register {
            "+" => Ok(&self.clipboard),
            "*" => Ok(&self.primary),
            _ => Err("Clipboard register must be + or *".to_owned()),
        }
    }

    fn command(&self, copy: bool, register: &str) -> Result<Command, String> {
        if register != "+" && register != "*" {
            return Err("Clipboard register must be + or *".to_owned());
        }
        let commands = self.commands.as_ref().map_err(Clone::clone)?;
        let mut command = Command::new(if copy {
            &commands.copy
        } else {
            &commands.paste
        });
        match commands.backend {
            Backend::Xclip => {
                command.args([
                    "-selection",
                    if register == "*" {
                        "primary"
                    } else {
                        "clipboard"
                    },
                ]);
                command.arg(if copy { "-in" } else { "-out" });
                if copy {
                    command.arg("-quiet");
                }
            }
            Backend::Xsel => {
                command.arg(if register == "*" {
                    "--primary"
                } else {
                    "--clipboard"
                });
                command.arg(if copy { "--input" } else { "--output" });
                if copy {
                    command.arg("--nodetach");
                }
            }
            Backend::Wayland => {
                if register == "*" {
                    command.arg("--primary");
                }
                command.arg(if copy { "--foreground" } else { "--no-newline" });
            }
            Backend::Pbcopy => {}
        }
        Ok(command)
    }

    pub(crate) fn copy_later(
        self: &Arc<Self>,
        text: String,
        register: String,
        report: impl FnOnce(String) + Send + 'static,
    ) -> Result<(), String> {
        use tokio::runtime::Handle;

        validate_text(&text)?;
        self.slot(&register)?;
        let runtime = Handle::try_current().map_err(|e| format!("Schedule clipboard copy: {e}"))?;
        // Register before checking cancellation so shutdown either waits for
        // this accepted job or prevents it from starting after its wait ends.
        let tracked = self.tasks.token();
        if self.stop.is_cancelled() {
            return Err("Clipboard exchange has shut down".into());
        }
        let (completion, next) = oneshot::channel();
        let previous = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(next);
        let clipboard = self.clone();
        runtime.spawn(async move {
            let _tracked = tracked;
            let _completion = completion;
            let operation = async {
                if let Some(previous) = previous {
                    // Dropping a predecessor's sender releases the queue on
                    // success, backend failure, panic, or cancellation.
                    let _ = previous.await;
                }
                clipboard.copy(text, &register).await
            };
            tokio::select! {
                biased;
                _ = clipboard.stop.cancelled() => {},
                ret = operation => {
                    if let Err(e) = ret
                        && !clipboard.stop.is_cancelled()
                    {
                        report(e);
                    }
                }
            }
        });
        Ok(())
    }

    pub(crate) fn cancel(&self) {
        self.stop.cancel();
    }

    /// Copies the supplied text without changing line endings or trailing LF.
    ///
    /// Rejects NUL and byte order marks under the plugin string contract.
    /// Completion waits for the backend to acquire or store the supplied text.
    pub async fn copy(&self, text: String, register: &str) -> Result<(), String> {
        validate_text(&text)?;
        let mut slot = self.slot(register)?.lock().await;
        if slot.closed || self.stop.is_cancelled() {
            return Err("Clipboard exchange has shut down".into());
        }
        let commands = self.commands.as_ref().map_err(Clone::clone)?;
        let mut command = self.command(true, register)?;
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        if commands.backend.retains_owner() {
            command.stderr(Stdio::null());
        } else {
            command.stderr(Stdio::piped());
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Start clipboard copy utility: {e}"))?;
        let Some(mut stdin) = child.stdin.take() else {
            return Err("Clipboard utility stdin is unavailable".into());
        };
        timeout(self.timeout, stdin.write_all(text.as_bytes()))
            .await
            .map_err(|_| "Write system clipboard timed out".to_owned())?
            .map_err(|e| format!("Write system clipboard: {e}"))?;
        drop(stdin);
        if commands.backend.retains_owner() {
            timeout(self.timeout, self.owner_ready(&mut child, &text, register))
                .await
                .map_err(|_| {
                    "Clipboard copy utility did not acquire its selection before timeout".to_owned()
                })??;
            if let Some(mut previous) = slot.owner.take() {
                stop(&mut previous).await?;
            }
            slot.owner = Some(child);
        } else {
            let output = timeout(self.timeout, child.wait_with_output())
                .await
                .map_err(|_| "Wait for clipboard copy timed out".to_owned())?
                .map_err(|e| format!("Wait for clipboard copy: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "Copy system clipboard: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok(())
    }

    /// Reads unmodified UTF-8 text from the selected clipboard backend.
    ///
    /// Preserves all line endings and trailing LF. Native GUI paste text is
    /// independent of the plugin string contract and may contain NUL or byte
    /// order marks.
    /// Utility I/O is bounded by the configured operation timeout.
    pub async fn read(&self, register: &str) -> Result<String, String> {
        let slot = self.slot(register)?;
        let operation = async {
            let slot = slot.lock().await;
            if slot.closed || self.stop.is_cancelled() {
                return Err("Clipboard exchange has shut down".into());
            }
            timeout(self.timeout, self.read_backend(register))
                .await
                .map_err(|_| "Read system clipboard timed out".to_owned())?
        };
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err("Clipboard exchange has shut down".into()),
            ret = operation => ret,
        }
    }

    async fn owner_ready(
        &self,
        child: &mut Child,
        text: &str,
        register: &str,
    ) -> Result<(), String> {
        use tokio::time::sleep;

        loop {
            let observed = self.read_backend(register).await;
            if observed.as_ref().is_ok_and(|observed| observed == text) {
                return Ok(());
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("Inspect clipboard utility: {e}"))?
            {
                return Err(observed
                    .err()
                    .unwrap_or_else(|| format!("Clipboard copy utility exited with {status}")));
            }
            sleep(Duration::from_millis(1)).await;
        }
    }

    async fn read_backend(&self, register: &str) -> Result<String, String> {
        let output = self
            .command(false, register)?
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("Read system clipboard: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "Read system clipboard: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8(output.stdout).map_err(|e| format!("Decode system clipboard text: {e}"))
    }

    /// Cancels queued copies, waits for their tasks, and stops clipboard owners.
    ///
    /// Rejects subsequent copies and reads, and reaps foreground owner utilities.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.cancel();
        self.tasks.close();
        self.tasks.wait().await;
        let mut first_error = None;
        for slot in [&self.clipboard, &self.primary] {
            let mut slot = slot.lock().await;
            slot.closed = true;
            if let Some(mut owner) = slot.owner.take()
                && let Err(e) = stop(&mut owner).await
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn validate_text(text: &str) -> Result<(), String> {
    if text.contains(['\0', '\u{feff}']) {
        Err("Clipboard text contains NUL or a byte order mark".into())
    } else {
        Ok(())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.stop.cancel();
        for slot in [&mut self.clipboard, &mut self.primary] {
            if let Some(owner) = slot.get_mut().owner.as_mut() {
                let _ = owner.start_kill();
            }
        }
    }
}

async fn stop(child: &mut Child) -> Result<(), String> {
    if let Err(e) = child.start_kill()
        && child
            .try_wait()
            .map_err(|e| format!("Inspect clipboard utility: {e}"))?
            .is_none()
    {
        return Err(format!("Stop clipboard utility: {e}"));
    }
    child
        .wait()
        .await
        .map_err(|e| format!("Reap clipboard utility: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;

    use tempfile::tempdir;

    fn configured(backend: Backend, copy: PathBuf, paste: PathBuf, timeout: Duration) -> Clipboard {
        Clipboard {
            commands: Ok(Commands {
                backend,
                copy,
                paste,
            }),
            timeout,
            clipboard: Mutex::default(),
            primary: Mutex::default(),
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
            queue: StdMutex::new(None),
        }
    }

    fn executable_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn validates_backend_dependencies_and_session_environment() {
        let dir = tempdir().unwrap();
        for name in ["xclip", "xsel", "wl-copy", "wl-paste", "pbcopy", "pbpaste"] {
            executable_file(dir.path(), name, "#!/bin/sh\nexit 0\n");
        }
        let env = Environment {
            path: Some(dir.path().as_os_str().to_owned()),
            display: Some("remote-display.test:0".into()),
            ..Environment::default()
        };
        for (name, expected) in [("xclip", Backend::Xclip), ("xsel", Backend::Xsel)] {
            assert_eq!(env.select(name).unwrap().backend, expected);
        }
        assert_eq!(
            env.select("auto").unwrap().backend,
            if cfg!(target_os = "macos") {
                Backend::Pbcopy
            } else {
                Backend::Xclip
            }
        );
        assert!(
            env.select("unknown")
                .err()
                .unwrap()
                .contains("Unknown clipboard backend")
        );
        let empty = Environment {
            path: env.path.clone(),
            ..Environment::default()
        };
        assert!(empty.select("xclip").err().unwrap().contains("DISPLAY"));
        assert!(empty.select("xsel").err().unwrap().contains("DISPLAY"));
        assert!(
            empty
                .select("wl_clipboard")
                .err()
                .unwrap()
                .contains("WAYLAND_DISPLAY")
        );
        fs::remove_file(dir.path().join("xclip")).unwrap();
        assert!(env.select("xclip").err().unwrap().contains("xclip"));
        if !cfg!(target_os = "macos") {
            assert_eq!(env.select("auto").unwrap().backend, Backend::Xsel);
            assert!(env.select("pbcopy").err().unwrap().contains("macOS"));
        }
        fs::set_permissions(dir.path().join("xsel"), {
            use std::os::unix::fs::PermissionsExt as _;
            fs::Permissions::from_mode(0o644)
        })
        .unwrap();
        assert!(env.select("xsel").err().unwrap().contains("xsel"));
    }

    #[test]
    fn constructs_commands_for_the_selected_backend_and_register() {
        fn check(backend: Backend, register: &str, copy: &[&str], paste: &[&str]) {
            let clipboard = configured(
                backend,
                "copy".into(),
                "paste".into(),
                Duration::from_secs(3),
            );
            for (is_copy, expected) in [(true, copy), (false, paste)] {
                let command = clipboard.command(is_copy, register).unwrap();
                let command = command.as_std();
                assert_eq!(
                    command.get_program(),
                    if is_copy { "copy" } else { "paste" }
                );
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    expected,
                    "{backend:?}, {register}"
                );
            }
        }
        check(
            Backend::Xclip,
            "+",
            &["-selection", "clipboard", "-in", "-quiet"],
            &["-selection", "clipboard", "-out"],
        );
        check(
            Backend::Xclip,
            "*",
            &["-selection", "primary", "-in", "-quiet"],
            &["-selection", "primary", "-out"],
        );
        check(
            Backend::Xsel,
            "+",
            &["--clipboard", "--input", "--nodetach"],
            &["--clipboard", "--output"],
        );
        check(
            Backend::Xsel,
            "*",
            &["--primary", "--input", "--nodetach"],
            &["--primary", "--output"],
        );
        check(Backend::Wayland, "+", &["--foreground"], &["--no-newline"]);
        check(
            Backend::Wayland,
            "*",
            &["--primary", "--foreground"],
            &["--primary", "--no-newline"],
        );
        check(Backend::Pbcopy, "+", &[], &[]);
        check(Backend::Pbcopy, "*", &[], &[]);
    }

    #[tokio::test]
    async fn exchanges_exact_unicode_text_and_line_endings_with_a_command_backend() {
        let dir = tempdir().unwrap();
        let contents = dir.path().join("contents");
        let copy = executable_file(
            dir.path(),
            "copy",
            &format!("#!/bin/sh\nexec /bin/cat > '{}'\n", contents.display()),
        );
        let paste = executable_file(
            dir.path(),
            "paste",
            &format!("#!/bin/sh\nexec /bin/cat '{}'\n", contents.display()),
        );
        let clipboard = configured(Backend::Pbcopy, copy, paste, Duration::from_secs(1));
        for register in ["+", "*"] {
            for text in [
                "",
                "\n",
                "\n\n",
                "one line",
                "one line\n",
                "日本語🙂\nliteral<Esc>",
                "日本語🙂\nliteral<Esc>\n\n",
            ] {
                clipboard.copy(text.to_owned(), register).await.unwrap();
                assert_eq!(
                    fs::read(&contents).unwrap(),
                    text.as_bytes(),
                    "{register}, {text:?}"
                );
                assert_eq!(
                    clipboard.read(register).await.unwrap(),
                    text,
                    "{register}, {text:?}"
                );
            }
        }
        for text in [
            "external\ntext",
            "external\r\ntext\n",
            "before\0after",
            "before\u{feff}after",
        ] {
            fs::write(&contents, text).unwrap();
            assert_eq!(clipboard.read("+").await.unwrap(), text);
        }
        clipboard.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stopping_an_old_attachment_preserves_new_clipboard_owners() {
        let dir = tempdir().unwrap();
        let contents = dir.path().join("contents");
        let copy = executable_file(
            dir.path(),
            "copy",
            &format!(
                "#!/bin/sh\n/bin/cat > '{}'\nexec /bin/sleep 60\n",
                contents.display()
            ),
        );
        let paste = executable_file(
            dir.path(),
            "paste",
            &format!("#!/bin/sh\nexec /bin/cat '{}'\n", contents.display()),
        );
        let old = configured(Backend::Xclip, copy, paste, Duration::from_secs(1));
        old.copy("old\n".into(), "+").await.unwrap();
        let new = old.fork();
        new.copy("new\n\n".into(), "+").await.unwrap();
        old.shutdown().await.unwrap();
        assert_eq!(new.read("+").await.unwrap(), "new\n\n");
        assert!(
            new.clipboard
                .lock()
                .await
                .owner
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        );
        new.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_copy_arguments_before_invoking_the_backend() {
        let dir = tempdir().unwrap();
        let invoked = dir.path().join("invoked");
        let program = executable_file(
            dir.path(),
            "copy",
            &format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 99\n",
                invoked.display()
            ),
        );
        let clipboard = configured(
            Backend::Pbcopy,
            program.clone(),
            program,
            Duration::from_secs(1),
        );
        for text in ["before\0after", "before\u{feff}after"] {
            assert_eq!(
                clipboard.copy(text.into(), "+").await,
                Err("Clipboard text contains NUL or a byte order mark".into())
            );
        }
        for register in ["", "invalid", "+\0", "*\u{feff}"] {
            assert_eq!(
                clipboard.copy("text".into(), register).await,
                Err("Clipboard register must be + or *".into())
            );
            assert_eq!(
                clipboard.read(register).await,
                Err("Clipboard register must be + or *".into())
            );
        }
        assert!(!invoked.exists());
    }

    #[tokio::test]
    async fn rejects_non_utf8_backend_text_for_native_paste() {
        let dir = tempdir().unwrap();
        let program = executable_file(dir.path(), "paste", "#!/bin/sh\nprintf '\\377'\n");
        let clipboard = configured(
            Backend::Pbcopy,
            program.clone(),
            program,
            Duration::from_secs(1),
        );
        assert!(
            clipboard
                .read("+")
                .await
                .unwrap_err()
                .contains("Decode system clipboard text")
        );
        assert_eq!(clipboard.backend(), Some("pbcopy"));
    }

    #[tokio::test]
    async fn respects_operation_timeout_and_retains_backend_after_runtime_failure() {
        let dir = tempdir().unwrap();
        let program = executable_file(dir.path(), "paste", "#!/bin/sh\nexec /bin/sleep 60\n");
        let clipboard = configured(
            Backend::Xclip,
            program.clone(),
            program.clone(),
            Duration::from_millis(10),
        );
        assert_eq!(
            clipboard.read("+").await.unwrap_err(),
            "Read system clipboard timed out"
        );
        assert_eq!(clipboard.backend(), Some("xclip"));
        fs::remove_file(program).unwrap();
        assert!(
            clipboard
                .read("+")
                .await
                .unwrap_err()
                .contains("Read system clipboard")
        );
        assert_eq!(clipboard.backend(), Some("xclip"));
        clipboard.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unavailable_backend_preserves_operation_errors_and_shutdown() {
        let clipboard = Clipboard {
            commands: Err("Clipboard backend xsel is unavailable: missing executable".into()),
            timeout: Duration::from_secs(3),
            clipboard: Mutex::default(),
            primary: Mutex::default(),
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
            queue: StdMutex::new(None),
        };
        assert_eq!(clipboard.backend(), None);
        for register in ["+", "*"] {
            assert_eq!(
                clipboard.read(register).await.unwrap_err(),
                "Clipboard backend xsel is unavailable: missing executable"
            );
            assert_eq!(
                clipboard.copy("text".into(), register).await.unwrap_err(),
                "Clipboard backend xsel is unavailable: missing executable"
            );
        }
        clipboard.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_registers_and_exchange_after_shutdown() {
        let clipboard = Clipboard::default();
        assert!(clipboard.copy("text".to_owned(), "invalid").await.is_err());
        assert!(clipboard.read("invalid").await.is_err());
        clipboard.shutdown().await.unwrap();
        for register in ["+", "*"] {
            assert!(clipboard.copy("text".to_owned(), register).await.is_err());
            assert!(clipboard.read(register).await.is_err());
        }
        clipboard.shutdown().await.unwrap();
    }

    #[test]
    fn queued_copy_validates_arguments_and_requires_a_runtime() {
        let clipboard = Arc::new(configured(
            Backend::Pbcopy,
            "copy".into(),
            "read".into(),
            Duration::from_secs(1),
        ));
        assert_eq!(
            clipboard.copy_later("invalid\0text".into(), "+".into(), |_| panic!(
                "Unexpected error callback"
            )),
            Err("Clipboard text contains NUL or a byte order mark".into())
        );
        assert_eq!(
            clipboard.copy_later("text".into(), "invalid".into(), |_| panic!(
                "Unexpected error callback"
            )),
            Err("Clipboard register must be + or *".into())
        );
        let error = clipboard
            .copy_later("text".into(), "+".into(), |_| {
                panic!("Unexpected error callback")
            })
            .unwrap_err();
        assert!(error.contains("Schedule clipboard copy"));
        assert!(clipboard.tasks.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_copies_retain_notification_order() {
        let dir = tempdir().unwrap();
        let contents = dir.path().join("contents");
        let copy = executable_file(
            dir.path(),
            "copy",
            &format!("#!/bin/sh\nexec /bin/cat > '{}'\n", contents.display()),
        );
        let read = executable_file(
            dir.path(),
            "read",
            &format!("#!/bin/sh\nexec /bin/cat '{}'\n", contents.display()),
        );
        let clipboard = Arc::new(configured(
            Backend::Pbcopy,
            copy,
            read,
            Duration::from_secs(1),
        ));
        let (errors, mut messages) = mpsc::unbounded_channel();
        for text in ["first\n", "second\n\n"] {
            let errors = errors.clone();
            clipboard
                .copy_later(text.to_owned(), "+".into(), move |e| {
                    let _ = errors.send(e);
                })
                .unwrap();
        }
        clipboard.tasks.close();
        timeout(Duration::from_secs(2), clipboard.tasks.wait())
            .await
            .unwrap();
        assert_eq!(clipboard.read("+").await.unwrap(), "second\n\n");
        assert!(messages.try_recv().is_err());
        clipboard.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queued_backend_failure_does_not_prevent_the_next_copy() {
        let dir = tempdir().unwrap();
        let contents = dir.path().join("contents");
        let copy = executable_file(
            dir.path(),
            "copy",
            &format!(
                "#!/bin/sh\ntext=$(/bin/cat)\nif [ \"$text\" = fail ]; then printf 'Backend failure' >&2; exit 7; fi\nprintf '%s' \"$text\" > '{}'\n",
                contents.display()
            ),
        );
        let read = executable_file(
            dir.path(),
            "read",
            &format!("#!/bin/sh\nexec /bin/cat '{}'\n", contents.display()),
        );
        let clipboard = Arc::new(configured(
            Backend::Pbcopy,
            copy,
            read,
            Duration::from_secs(1),
        ));
        let (errors, mut messages) = mpsc::unbounded_channel();
        for text in ["fail", "later"] {
            let errors = errors.clone();
            clipboard
                .copy_later(text.to_owned(), "+".into(), move |e| {
                    let _ = errors.send(e);
                })
                .unwrap();
        }
        clipboard.tasks.close();
        timeout(Duration::from_secs(2), clipboard.tasks.wait())
            .await
            .unwrap();
        assert_eq!(
            messages.try_recv().unwrap(),
            "Copy system clipboard: Backend failure"
        );
        assert!(messages.try_recv().is_err());
        assert_eq!(clipboard.read("+").await.unwrap(), "later");
        clipboard.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_queued_copies_without_backend_writes_or_errors() {
        let dir = tempdir().unwrap();
        let contents = dir.path().join("contents");
        let copy = executable_file(
            dir.path(),
            "copy",
            &format!("#!/bin/sh\nexec /bin/cat > '{}'\n", contents.display()),
        );
        let clipboard = Arc::new(configured(
            Backend::Pbcopy,
            copy.clone(),
            copy,
            Duration::from_secs(1),
        ));
        let (errors, mut messages) = mpsc::unbounded_channel();
        let held = clipboard.clipboard.lock().await;
        for text in ["first", "second"] {
            let errors = errors.clone();
            clipboard
                .copy_later(text.to_owned(), "+".into(), move |e| {
                    let _ = errors.send(e);
                })
                .unwrap();
        }
        clipboard.cancel();
        drop(held);
        timeout(Duration::from_secs(1), clipboard.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(clipboard.tasks.is_empty());
        assert!(!contents.exists());
        assert!(messages.try_recv().is_err());
        assert_eq!(
            clipboard.copy_later("late".into(), "+".into(), |_| panic!(
                "Unexpected error callback"
            )),
            Err("Clipboard exchange has shut down".into())
        );
        assert!(!contents.exists());
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_pending_backend_read() {
        use tokio::spawn;
        use tokio::task::yield_now;

        timeout(Duration::from_secs(3), async {
            let dir = tempdir().unwrap();
            let started = dir.path().join("started");
            let read = executable_file(
                dir.path(),
                "read",
                &format!(
                    "#!/bin/sh\nprintf started > '{}'\nexec /bin/sleep 60\n",
                    started.display()
                ),
            );
            let clipboard = Arc::new(configured(
                Backend::Pbcopy,
                read.clone(),
                read,
                Duration::from_secs(60),
            ));
            let operation = {
                let clipboard = clipboard.clone();
                spawn(async move { clipboard.read("+").await })
            };
            while !started.exists() {
                yield_now().await;
            }
            clipboard.cancel();
            let result = timeout(Duration::from_secs(1), operation)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result, Err("Clipboard exchange has shut down".into()));
            clipboard.shutdown().await.unwrap();
        })
        .await
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "Requires xclip, DISPLAY, and permission to access the X11 clipboard; preserves existing selection bytes"]
    async fn round_trips_exact_x11_text_and_reaps_owners() {
        use std::io::Write as _;
        struct Restore {
            selections: Vec<(&'static str, Vec<u8>)>,
            restored: bool,
        }
        impl Restore {
            fn restore(&mut self) -> Result<(), String> {
                use std::process::Command as StdCommand;

                for (selection, bytes) in &self.selections {
                    let mut child = StdCommand::new("xclip")
                        .args(["-selection", selection, "-in"])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_err(|e| e.to_string())?;
                    if let Some(mut stdin) = child.stdin.take() {
                        stdin.write_all(bytes).map_err(|e| e.to_string())?;
                    }
                    if !child.wait().map_err(|e| e.to_string())?.success() {
                        return Err("Restore original clipboard failed".to_owned());
                    }
                }
                self.restored = true;
                Ok(())
            }
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                if !self.restored {
                    let _ = self.restore();
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
        let mut restore = Restore {
            selections: vec![
                ("clipboard", raw("clipboard").await),
                ("primary", raw("primary").await),
            ],
            restored: false,
        };
        let clipboard = Clipboard::default();
        for register in ["+", "*"] {
            for text in [
                "",
                "\n",
                "日本語🙂\n<Esc> literal",
                "日本語🙂\n<Esc> literal\n",
                "first\n終端\n\n",
            ] {
                timeout(
                    Duration::from_secs(5),
                    clipboard.copy(text.to_owned(), register),
                )
                .await
                .expect("Clipboard copy exceeded five seconds")
                .unwrap();
                let observed = timeout(Duration::from_secs(5), clipboard.read(register))
                    .await
                    .expect("Clipboard read exceeded five seconds")
                    .unwrap();
                assert_eq!(observed, text, "{register}, {text:?}");
            }
        }
        clipboard.copy("clipboard\n".to_owned(), "+").await.unwrap();
        clipboard.copy("primary\n\n".to_owned(), "*").await.unwrap();
        assert_eq!(clipboard.read("+").await.unwrap(), "clipboard\n");
        assert_eq!(clipboard.read("*").await.unwrap(), "primary\n\n");
        let mut owners = Vec::new();
        for register in ["+", "*"] {
            owners.push(
                clipboard
                    .slot(register)
                    .unwrap()
                    .lock()
                    .await
                    .owner
                    .as_ref()
                    .unwrap()
                    .id()
                    .unwrap(),
            );
        }
        timeout(Duration::from_secs(5), clipboard.shutdown())
            .await
            .expect("Clipboard owner shutdown exceeded five seconds")
            .unwrap();
        for pid in owners {
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "Clipboard utility was not reaped"
            );
        }
        restore.restore().unwrap();
        for (selection, original) in &restore.selections {
            assert!(
                raw(selection).await == *original,
                "Original clipboard bytes were not restored"
            );
        }
    }
}
