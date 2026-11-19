#[cfg(feature = "ssh")]
use super::Remote;
use super::path;
use super::{Data, Kind, Registry, Resource, ResourceHandle, Resources};

#[cfg(feature = "ssh")]
use tokio::io::AsyncRead;
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;

use std::collections::{BTreeSet, HashSet};
#[cfg(feature = "ssh")]
use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub(super) struct Directories {
    created: BTreeSet<PathBuf>,
    active: Vec<(PathBuf, Arc<AtomicBool>)>,
}

struct Running(Arc<AtomicBool>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Destination<'p> {
    base: &'p Path,
    registry: &'p Arc<Registry>,
    path: &'p Path,
    original: &'p Path,
}

impl Resources {
    /// Schedules a file or directory copy without waiting for filesystem I/O.
    pub async fn copy(
        &self,
        client_file: String,
        host_file: String,
        overwrite: bool,
    ) -> Result<ResourceHandle, String> {
        use tokio::spawn;

        path::valid_string(&host_file)?;
        let destination = self.client_path(&client_file)?;
        let original = self.base.join(client_file.trim_start_matches('/'));
        // Before the worker inspects the destination, cancellation must preserve
        // a potentially pre-existing entry when replacement was not requested.
        let preserve = Arc::new(AtomicBool::new(!overwrite));
        let mut resource = Resource::new(
            Data::Copy {
                path: destination.clone(),
                preserve: preserve.clone(),
            },
            &self.stop,
            &self.owner,
        );
        let stop = resource.stop.clone();
        let errors = self.errors.clone();
        let base = self.base.clone();
        let registry = self.registry.clone();
        let running = Arc::new(AtomicBool::new(true));
        registry
            .directories
            .lock()
            .await
            .active
            .push((destination.clone(), running.clone()));
        let running = Running(running);
        #[cfg(feature = "ssh")]
        let remote = self.remote.clone();
        resource.task = Some(spawn(async move {
            let _running = running;
            #[cfg(feature = "ssh")]
            if let Some(remote) = remote {
                if let Err(e) = remote_copy(
                    remote,
                    &host_file,
                    Destination {
                        base: &base,
                        registry: &registry,
                        path: &destination,
                        original: &original,
                    },
                    overwrite,
                    preserve,
                    stop,
                )
                .await
                {
                    let _ = errors.send(format!(
                        "Copy host path {host_file} to {}: {e}",
                        destination.display()
                    ));
                }
                return;
            }
            let target = destination.clone();
            let src = host_file.clone();
            let ret = spawn_blocking(move || {
                local_copy(
                    Path::new(&src),
                    Destination {
                        base: &base,
                        registry: &registry,
                        path: &target,
                        original: &original,
                    },
                    overwrite,
                    &preserve,
                    &stop,
                )
            })
            .await;
            match ret {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = errors.send(format!(
                        "Copy host path {host_file} to {}: {e}",
                        destination.display()
                    ));
                }
                Err(e) => {
                    let _ = errors.send(format!("Copy worker failed: {e}"));
                }
            }
        }));
        self.insert(resource).await
    }

    /// Stops a copy before deleting its owned destination.
    pub async fn clear_copy(&self, handle: ResourceHandle) {
        if let Some(resource) = self.take(handle, &[Kind::Copy]).await {
            self.stop(resource).await;
        }
    }
}

fn existing(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn check_original(path: &Path) -> io::Result<()> {
    if existing(path)?.is_some_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Original destination entry is a symbolic link",
        ));
    }
    Ok(())
}

/// Checks ancestors without following a final destination symlink.
fn check_parent(base: &Path, path: &Path) -> io::Result<()> {
    path::descendant(base, path).map(|_| ())
}

fn prepare(
    base: &Path,
    registry: &Registry,
    path: &Path,
    directory: bool,
    overwrite: bool,
) -> io::Result<bool> {
    check_parent(base, path)?;
    if let Some(metadata) = existing(path)? {
        if metadata.file_type().is_symlink()
            || metadata.is_dir() != directory
            || (!directory && !metadata.is_file())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Destination type does not match the source",
            ));
        }
        if !overwrite {
            return Ok(false);
        }
        if directory {
            delete_sync(base, path)?;
        }
    }
    create_parents(base, registry, path)?;
    if directory {
        fs::create_dir(path)?;
    }
    Ok(true)
}

fn create_parents(base: &Path, registry: &Registry, path: &Path) -> io::Result<()> {
    let base = fs::canonicalize(base)?;
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Copy destination has no parent",
        ));
    };
    let parent = path::resolve(parent)?;
    let relative = parent.strip_prefix(&base).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Copy parent is outside the process temporary directory",
        )
    })?;
    let mut path = base.clone();
    let mut directories = registry.directories.blocking_lock();
    for component in relative.components() {
        path.push(component);
        match fs::create_dir(&path) {
            Ok(()) => {
                directories.created.insert(path.clone());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::metadata(&path)?.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "Copy parent is not a directory",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl Resources {
    pub(super) async fn prune_dirs(&self) {
        use std::mem;

        let registry = self.registry.clone();
        let base = self.base.clone();
        match spawn_blocking(move || {
            let mut directories = registry.directories.blocking_lock();
            directories
                .active
                .retain(|(_, running)| running.load(Ordering::Acquire));
            let mut pending = mem::take(&mut directories.created);
            let mut errors = Vec::new();
            while let Some(path) = pending.pop_last() {
                if directories
                    .active
                    .iter()
                    .any(|(target, _)| target.starts_with(&path))
                {
                    directories.created.insert(path);
                    continue;
                }
                if path::descendant(&base, &path).is_err() {
                    continue;
                }
                match fs::read_dir(&path) {
                    Ok(mut entries) => match entries.next() {
                        Some(Ok(_)) => {
                            directories.created.insert(path);
                            continue;
                        }
                        Some(Err(e)) => {
                            errors.push(format!(
                                "Inspect generated copy directory {}: {e}",
                                path.display()
                            ));
                            continue;
                        }
                        None => {}
                    },
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        errors.push(format!(
                            "Inspect generated copy directory {}: {e}",
                            path.display()
                        ));
                        continue;
                    }
                }
                match fs::remove_dir(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    // New contents make the directory ineligible for pruning.
                    Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => {
                        directories.created.insert(path);
                    }
                    Err(e) => errors.push(format!(
                        "Remove generated copy directory {}: {e}",
                        path.display()
                    )),
                }
            }
            errors
        })
        .await
        {
            Ok(errors) => {
                for e in errors {
                    self.report(e);
                }
            }
            Err(e) => self.report(format!("Prune generated copy directories: {e}")),
        }
    }
}

fn local_copy(
    source: &Path,
    destination: Destination<'_>,
    overwrite: bool,
    preserve: &AtomicBool,
    stop: &CancellationToken,
) -> io::Result<()> {
    let Destination {
        base,
        registry,
        path: destination,
        original,
    } = destination;
    if stop.is_cancelled() {
        return Ok(());
    }
    preserve.store(
        !overwrite && existing(destination)?.is_some(),
        Ordering::Release,
    );
    let source = fs::canonicalize(source)?;
    if fs::metadata(&source)?.is_dir() && resolved_destination(destination)?.starts_with(&source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Copy destination is inside the source directory",
        ));
    }
    enum Work {
        Entry(PathBuf, PathBuf, bool),
        Leave(PathBuf),
    }
    let mut pending = vec![Work::Entry(source, destination.to_owned(), overwrite)];
    let mut ancestors = HashSet::new();
    let mut buf = [0_u8; 65536];
    while let Some(work) = pending.pop() {
        if stop.is_cancelled() {
            return Ok(());
        }
        let (src, dst, overwrite) = match work {
            Work::Entry(src, dst, overwrite) => (src, dst, overwrite),
            Work::Leave(path) => {
                ancestors.remove(&path);
                continue;
            }
        };
        let src = fs::canonicalize(src)?;
        let metadata = fs::metadata(&src)?;
        if !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        if dst == destination {
            check_original(original)?;
        }
        if !prepare(base, registry, &dst, metadata.is_dir(), overwrite)? {
            continue;
        }
        if metadata.is_dir() {
            if !ancestors.insert(src.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Source directory contains a symbolic-link cycle",
                ));
            }
            pending.push(Work::Leave(src.clone()));
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                pending.push(Work::Entry(entry.path(), dst.join(entry.file_name()), true));
            }
        } else {
            use std::os::unix::fs::MetadataExt as _;
            let mut rdr = fs::File::open(src)?;
            let source_metadata = rdr.metadata()?;
            if existing(&dst)?.is_some_and(|metadata| {
                metadata.dev() == source_metadata.dev() && metadata.ino() == source_metadata.ino()
            }) {
                continue;
            }
            let mut wtr = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(dst)?;
            loop {
                if stop.is_cancelled() {
                    break;
                }
                let n = rdr.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                // `read` returns at most the buffer's length. `get` keeps that
                // relationship checked when the buffer or reader changes.
                if let Some(bytes) = buf.get(..n) {
                    wtr.write_all(bytes)?;
                }
            }
        }
    }
    Ok(())
}

fn resolved_destination(path: &Path) -> io::Result<PathBuf> {
    path::resolve(path)
}

fn delete_sync(base: &Path, path: &Path) -> io::Result<()> {
    check_parent(base, path)?;
    let Some(metadata) = existing(path)? else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

pub(super) async fn delete_destination(base: &Path, path: &Path) -> io::Result<()> {
    let base = base.to_owned();
    let path = path.to_owned();
    spawn_blocking(move || delete_sync(&base, &path))
        .await
        .map_err(io::Error::other)?
}

#[cfg(feature = "ssh")]
async fn remote_step<T, E: fmt::Display>(
    stop: &CancellationToken,
    future: impl Future<Output = Result<T, E>>,
) -> Result<Option<T>, String> {
    tokio::select! {
        biased;
        _ = stop.cancelled() => Ok(None),
        ret = future => ret.map(Some).map_err(|e| e.to_string()),
    }
}

#[cfg(feature = "ssh")]
async fn remote_copy(
    remote: Remote,
    source: &str,
    destination: Destination<'_>,
    overwrite: bool,
    preserve: Arc<AtomicBool>,
    stop: CancellationToken,
) -> Result<(), String> {
    use russh_sftp::client::SftpSession;

    let Destination {
        base,
        registry,
        path: destination,
        original,
    } = destination;
    if stop.is_cancelled() {
        return Ok(());
    }
    preserve.store(
        !overwrite && existing(destination).map_err(|e| e.to_string())?.is_some(),
        Ordering::Release,
    );
    let Some(channel) = remote_step(&stop, remote.handle().channel_open_session()).await? else {
        return Ok(());
    };
    if remote_step(&stop, channel.request_subsystem(true, "sftp"))
        .await?
        .is_none()
    {
        return Ok(());
    }
    let Some(sftp) = remote_step(&stop, SftpSession::new(channel.into_stream())).await? else {
        return Ok(());
    };
    enum Work {
        Entry(String, PathBuf, bool),
        Leave(String),
    }
    let mut pending = vec![Work::Entry(
        source.to_owned(),
        destination.to_owned(),
        overwrite,
    )];
    let mut ancestors = HashSet::new();
    let mut buf = [0_u8; 65536];
    while let Some(work) = pending.pop() {
        if stop.is_cancelled() {
            break;
        }
        let (src, dst, overwrite) = match work {
            Work::Entry(src, dst, overwrite) => (src, dst, overwrite),
            Work::Leave(src) => {
                ancestors.remove(&src);
                continue;
            }
        };
        let Some(src) = remote_step(&stop, sftp.canonicalize(src)).await? else {
            break;
        };
        let Some(metadata) = remote_step(&stop, sftp.metadata(src.clone())).await? else {
            break;
        };
        let file_type = metadata.file_type();
        if !file_type.is_dir() && !file_type.is_file() {
            continue;
        }
        if dst == destination {
            check_original(original).map_err(|e| e.to_string())?;
        }
        let prepared = {
            let base = base.to_owned();
            let dst = dst.clone();
            let registry = registry.clone();
            spawn_blocking(move || prepare(&base, &registry, &dst, file_type.is_dir(), overwrite))
                .await
                .map_err(|e| format!("Prepare copy destination: {e}"))?
                .map_err(|e| e.to_string())?
        };
        if !prepared {
            continue;
        }
        if file_type.is_dir() {
            if !ancestors.insert(src.clone()) {
                return Err("Source directory contains a symbolic-link cycle".to_owned());
            }
            pending.push(Work::Leave(src.clone()));
            let Some(entries) = remote_step(&stop, sftp.read_dir(src.clone())).await? else {
                break;
            };
            for entry in entries {
                let name = entry.file_name();
                if name != "." && name != ".." {
                    pending.push(Work::Entry(format!("{src}/{name}"), dst.join(name), true));
                }
            }
        } else {
            let Some(mut rdr) = remote_step(&stop, sftp.open(src)).await? else {
                break;
            };
            staged_copy(&mut rdr, &dst, &mut buf, &stop).await?;
        }
    }
    if stop.is_cancelled() {
        Ok(())
    } else {
        sftp.close().await.map_err(|e| e.to_string())
    }
}

#[cfg(feature = "ssh")]
async fn staged_copy(
    rdr: &mut (impl AsyncRead + Unpin),
    destination: &Path,
    buf: &mut [u8],
    stop: &CancellationToken,
) -> Result<(), String> {
    use tempfile::{Builder, NamedTempFile};

    use tokio::fs::File;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    if stop.is_cancelled() {
        return Ok(());
    }
    let parent = destination
        .parent()
        .ok_or("Copy destination has no parent")?
        .to_owned();
    let (file, stage) = spawn_blocking(move || {
        Builder::new()
            .prefix(".nauvim-copy-")
            .tempfile_in(parent)
            .map(NamedTempFile::into_parts)
    })
    .await
    .map_err(|e| format!("Create copy staging file: {e}"))?
    .map_err(|e| format!("Create copy staging file: {e}"))?;
    let mut wtr = File::from_std(file);
    let transfer = async {
        loop {
            let Some(n) = remote_step(stop, rdr.read(buf)).await? else {
                return Ok::<_, String>(false);
            };
            if n == 0 {
                return Ok(true);
            }
            let Some(bytes) = buf.get(..n) else {
                return Err("Copy reader returned an invalid byte count".to_owned());
            };
            wtr.write_all(bytes).await.map_err(|e| e.to_string())?;
            // Complete the background write before observing cancellation or
            // removing the staging file. No write can outlive this worker.
            wtr.flush().await.map_err(|e| e.to_string())?;
        }
    }
    .await;
    drop(wtr);
    let commit = matches!(transfer, Ok(true)) && !stop.is_cancelled();
    let destination = destination.to_owned();
    let finish = spawn_blocking(move || {
        if commit {
            match stage.persist(destination) {
                Ok(()) => Ok(()),
                Err(e) => {
                    let message = format!("Install copied file: {}", e.error);
                    match e.path.close() {
                        Ok(()) => Err(message),
                        Err(e) => Err(format!("{message}; remove copy staging file: {e}")),
                    }
                }
            }
        } else {
            stage
                .close()
                .map_err(|e| format!("Remove copy staging file: {e}"))
        }
    })
    .await
    .map_err(|e| format!("Finish copied file: {e}"))?;
    match (transfer, finish) {
        (Err(e), Err(cleanup)) => Err(format!("{e}; {cleanup}")),
        (Err(e), Ok(())) => Err(e),
        (Ok(_), ret) => ret,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use tokio::sync::mpsc;
    use tokio::task::yield_now;
    use tokio::time::{Duration, timeout};

    use std::os::unix::fs::symlink;

    fn run(base: &Path, src: &Path, dst: &Path, overwrite: bool) -> io::Result<()> {
        let registry = Arc::new(Registry::default());
        local_copy(
            src,
            Destination {
                base,
                registry: &registry,
                path: dst,
                original: dst,
            },
            overwrite,
            &AtomicBool::new(false),
            &CancellationToken::new(),
        )
    }

    #[test]
    fn copies_symlink_targets_and_replaces_directories() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        let base = dir.path().join("client");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::create_dir_all(base.join("dst")).unwrap();
        fs::write(src.join("nested/file"), b"contents").unwrap();
        symlink("nested/file", src.join("link")).unwrap();
        fs::write(base.join("dst/old"), b"old").unwrap();
        run(&base, &src, &base.join("dst"), true).unwrap();
        assert_eq!(fs::read(base.join("dst/nested/file")).unwrap(), b"contents");
        assert_eq!(fs::read(base.join("dst/link")).unwrap(), b"contents");
        assert!(
            !fs::symlink_metadata(base.join("dst/link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!base.join("dst/old").exists());
        fs::write(base.join("dst/link"), b"keep").unwrap();
        run(&base, &src, &base.join("dst"), false).unwrap();
        assert_eq!(fs::read(base.join("dst/link")).unwrap(), b"keep");
    }

    #[test]
    fn rejects_destination_symlinks_and_source_cycles() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        let base = dir.path().join("client");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&base).unwrap();
        fs::write(src.join("file"), b"contents").unwrap();
        symlink(src.join("file"), base.join("link")).unwrap();
        for overwrite in [true, false] {
            assert_eq!(
                run(&base, &src.join("file"), &base.join("link"), overwrite)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        symlink(".", src.join("cycle")).unwrap();
        assert_eq!(
            run(&base, &src, &base.join("dst"), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn clear_waits_for_copy_and_preserves_preexisting_destinations() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        let base = dir.path().join("client");
        fs::create_dir_all(&base).unwrap();
        fs::write(&src, b"replacement").unwrap();
        fs::write(base.join("keep"), b"original").unwrap();
        let (errors, _messages) = mpsc::unbounded_channel();
        let resources = Resources::new(base.clone(), errors);
        let keep = resources
            .copy("keep".to_owned(), src.to_string_lossy().into_owned(), false)
            .await
            .unwrap();
        resources.clear_copy(keep).await;
        assert_eq!(fs::read(base.join("keep")).unwrap(), b"original");
        let generated = resources
            .copy("/new".to_owned(), src.to_string_lossy().into_owned(), true)
            .await
            .unwrap();
        resources.clear_copy(generated).await;
        assert!(!base.join("new").exists());
        yield_now().await;
        assert!(!base.join("new").exists());
        assert_eq!(
            delete_destination(&base, &base).await.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(base.is_dir());
        assert_eq!(fs::read(base.join("keep")).unwrap(), b"original");
    }

    #[test]
    fn copying_a_file_onto_itself_or_a_hard_link_retains_its_bytes() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let source = base.join("source");
        fs::write(&source, b"contents").unwrap();
        run(base, &source, &source, true).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"contents");
        let linked = base.join("linked");
        fs::hard_link(&source, &linked).unwrap();
        run(base, &source, &linked, true).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"contents");
        assert_eq!(fs::read(&linked).unwrap(), b"contents");
    }

    #[tokio::test]
    async fn resolves_client_paths_but_rejects_original_destination_symlinks() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("source");
        let base = dir.path().join("client");
        fs::create_dir_all(&base).unwrap();
        fs::write(&src, b"replacement").unwrap();
        fs::write(base.join("target"), b"original").unwrap();
        symlink("target", base.join("link")).unwrap();
        let (errors, mut messages) = mpsc::unbounded_channel();
        let resources = Resources::new(base.clone(), errors);
        for overwrite in [false, true] {
            let handle = resources
                .copy(
                    "link".to_owned(),
                    src.to_str().unwrap().to_owned(),
                    overwrite,
                )
                .await
                .unwrap();
            let error = timeout(Duration::from_secs(5), messages.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(error.contains("symbolic link"));
            assert_eq!(fs::read(base.join("target")).unwrap(), b"original");
            if !overwrite {
                resources.clear_copy(handle).await;
                assert_eq!(fs::read(base.join("target")).unwrap(), b"original");
            }
        }
    }

    #[tokio::test]
    async fn clears_generated_parent_directories_but_preserves_preexisting_parents() {
        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            let base = dir.path().join("client");
            fs::create_dir_all(base.join("existing")).unwrap();
            let source = dir.path().join("source");
            fs::write(&source, b"contents").unwrap();
            let (errors, mut messages) = mpsc::unbounded_channel();
            let resources = Resources::new(base.clone(), errors);
            let handle = resources
                .copy(
                    "existing/generated/nested/file".to_owned(),
                    source.to_str().unwrap().to_owned(),
                    true,
                )
                .await
                .unwrap();
            while !base.join("existing/generated/nested/file").exists() {
                yield_now().await;
            }
            resources.clear_copy(handle).await;
            assert!(base.join("existing").is_dir());
            assert!(!base.join("existing/generated").exists());
            assert!(messages.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prunes_shared_generated_parents_after_the_last_attachment_cleans_up() {
        use std::sync::atomic::AtomicI64;

        timeout(Duration::from_secs(5), async {
            let dir = tempdir().unwrap();
            let base = dir.path().join("client");
            fs::create_dir(&base).unwrap();
            let source = dir.path().join("source");
            fs::write(&source, b"contents").unwrap();
            let (errors, mut messages) = mpsc::unbounded_channel();
            let handles = Arc::new(AtomicI64::new(0));
            let registry = Arc::new(Registry::default());
            let old = Resources::with_registry(
                base.clone(),
                errors.clone(),
                handles.clone(),
                registry.clone(),
            );
            let new = Resources::with_registry(base.clone(), errors, handles, registry);
            old.copy(
                "shared/old".to_owned(),
                source.to_str().unwrap().to_owned(),
                true,
            )
            .await
            .unwrap();
            new.copy(
                "shared/new".to_owned(),
                source.to_str().unwrap().to_owned(),
                true,
            )
            .await
            .unwrap();
            while !base.join("shared/old").exists() || !base.join("shared/new").exists() {
                yield_now().await;
            }
            old.cleanup().await;
            assert!(!base.join("shared/old").exists());
            assert_eq!(fs::read(base.join("shared/new")).unwrap(), b"contents");
            new.cleanup().await;
            assert!(!base.join("shared").exists());
            assert!(base.is_dir());
            assert!(messages.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[cfg(feature = "ssh")]
    #[tokio::test]
    async fn staged_copy_preserves_sources_that_alias_the_destination() {
        use tokio::fs::File;

        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, b"contents").unwrap();
        let linked = dir.path().join("linked");
        fs::hard_link(&source, &linked).unwrap();
        let mut buf = [0; 3];
        for destination in [&linked, &source] {
            let mut rdr = File::open(&source).await.unwrap();
            staged_copy(&mut rdr, destination, &mut buf, &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(fs::read(&source).unwrap(), b"contents");
            assert_eq!(fs::read(destination).unwrap(), b"contents");
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[cfg(feature = "ssh")]
    #[tokio::test]
    async fn staged_copy_removes_partial_files_after_read_errors() {
        use tokio::io::{AsyncRead, ReadBuf};

        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct Reader(bool);
        impl AsyncRead for Reader {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                if self.0 {
                    return Poll::Ready(Err(io::Error::other("Source read failed")));
                }
                self.0 = true;
                buf.put_slice(b"part");
                Poll::Ready(Ok(()))
            }
        }
        let dir = tempdir().unwrap();
        let destination = dir.path().join("destination");
        fs::write(&destination, b"original").unwrap();
        let mut buf = [0; 8];
        let error = staged_copy(
            &mut Reader(false),
            &destination,
            &mut buf,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.contains("Source read failed"));
        assert_eq!(fs::read(destination).unwrap(), b"original");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
