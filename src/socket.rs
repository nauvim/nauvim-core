use crate::config::{local_identity, stable_hash};
use crate::{Error, HostInfo};

use libc::{c_char, geteuid, sockaddr_un, uid_t};
use tokio::fs;

use std::env;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

pub(crate) const DIR_PREFIX: &str = "/tmp/nauvim-sockets-";
const MAX_PATH_LEN: usize = pathname_limit(|addr| addr.sun_path);

// Infer the platform's sun_path capacity from its array type and reserve a NUL byte.
const fn pathname_limit<const N: usize>(_: fn(sockaddr_un) -> [c_char; N]) -> usize {
    N.saturating_sub(1)
}

pub(crate) enum Destination<'a> {
    Default,
    Local(&'a str),
    #[cfg(feature = "ssh")]
    Ssh(&'a str),
}

pub(crate) fn file_name(
    client: Option<&HostInfo>,
    destination: Destination<'_>,
) -> Result<String, Error> {
    let local;
    let (host_name, user) = if let Some(info) = client {
        (info.host_name.as_str(), info.user.as_str())
    } else {
        local = local_identity()?;
        let (host_name, user) = &local;
        (host_name.as_str(), user.as_str())
    };
    let (kind, alias) = match destination {
        Destination::Default => ("default", ""),
        Destination::Local(alias) => ("local", alias),
        #[cfg(feature = "ssh")]
        Destination::Ssh(alias) => ("ssh", alias),
    };
    let identity = [host_name, user, kind, alias]
        .into_iter()
        .flat_map(|part| part.bytes().chain([0]));
    Ok(format!("{:016x}.sock", stable_hash(identity)))
}

pub(crate) async fn local_path(
    info: Option<&HostInfo>,
    alias: Option<&str>,
) -> Result<PathBuf, Error> {
    let name = file_name(info, alias.map_or(Destination::Default, Destination::Local))?;
    let uid = unsafe { geteuid() };
    let dir = PathBuf::from(format!("{DIR_PREFIX}{uid}"));
    prepare_local_dir(&dir, uid).await?;
    Ok(dir.join(name))
}

async fn prepare_local_dir(dir: &Path, uid: uid_t) -> Result<(), Error> {
    match fs::DirBuilder::new().mode(0o700).create(dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(Error::Message(format!(
                "Create Neovim socket directory {}: {e}",
                dir.display()
            )));
        }
    }

    let meta = fs::symlink_metadata(dir).await?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o7777 != 0o700 {
        return Err(Error::Message(format!(
            "Neovim socket directory {} must be owned by the effective user and have mode 0700",
            dir.display()
        )));
    }
    Ok(())
}

pub(crate) fn check_local_path_length(path: &Path, alias: Option<&str>) -> Result<(), Error> {
    let len = path.as_os_str().as_bytes().len();
    if len <= MAX_PATH_LEN {
        return Ok(());
    }

    let os = match env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        os => os,
    };
    let path = path.display();
    Err(Error::Message(match alias {
        Some(alias) => format!(
            "The Neovim socket path for host {alias:?} is too long ({len} bytes, maximum {MAX_PATH_LEN} on {os}). Set hosts[{alias:?}].socket to a shorter path.\n{path}"
        ),
        None => format!(
            "The Neovim socket path is too long ({len} bytes, maximum {MAX_PATH_LEN} on {os}).\n{path}"
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ProjectDirs;

    use tempfile::tempdir;

    use std::fs::Permissions;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::os::unix::net::SocketAddr;

    #[test]
    fn checks_the_platform_pathname_boundary() {
        for (len, fits) in [(MAX_PATH_LEN, true), (MAX_PATH_LEN + 1, false)] {
            let path = "x".repeat(len);
            let path = Path::new(&path);
            assert_eq!(SocketAddr::from_pathname(path).is_ok(), fits);
            assert_eq!(check_local_path_length(path, None).is_ok(), fits);
        }
    }

    #[test]
    fn default_names_depend_on_client_and_host_selection() {
        let mut client = HostInfo {
            host_name: "client".into(),
            user: "user".into(),
            config_file: None,
            home_dir: "/home/user".into(),
            project_dirs: ProjectDirs {
                cache_dir: "/cache".into(),
                data_dir: "/data".into(),
                runtime_dir: "/runtime".into(),
            },
        };
        let name = file_name(Some(&client), Destination::Local("work")).unwrap();
        client.project_dirs.runtime_dir = "/another/runtime".into();
        assert_eq!(
            file_name(Some(&client), Destination::Local("work")).unwrap(),
            name
        );
        assert_ne!(
            file_name(Some(&client), Destination::Local("home")).unwrap(),
            name
        );
        assert_ne!(
            file_name(Some(&client), Destination::Default).unwrap(),
            file_name(Some(&client), Destination::Local("")).unwrap(),
        );
        #[cfg(feature = "ssh")]
        assert_ne!(
            file_name(Some(&client), Destination::Ssh("work")).unwrap(),
            name
        );

        client.user = "another-user".into();
        assert_ne!(
            file_name(Some(&client), Destination::Local("work")).unwrap(),
            name
        );
    }

    #[tokio::test]
    async fn prepares_private_socket_directories_and_rejects_existing_symlinks() {
        let root = tempdir().unwrap();
        let dir = root.path().join("private");
        let uid = fs::metadata(root.path()).await.unwrap().uid();
        let (first, second) =
            tokio::join!(prepare_local_dir(&dir, uid), prepare_local_dir(&dir, uid));
        first.unwrap();
        second.unwrap();
        assert_eq!(fs::metadata(&dir).await.unwrap().mode() & 0o7777, 0o700);

        let link = root.path().join("link");
        symlink(&dir, &link).unwrap();
        assert!(prepare_local_dir(&link, uid).await.is_err());
        fs::set_permissions(&dir, Permissions::from_mode(0o755))
            .await
            .unwrap();
        assert!(prepare_local_dir(&dir, uid).await.is_err());
    }
}
