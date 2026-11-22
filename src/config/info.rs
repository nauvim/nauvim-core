use crate::Error;

use super::validate_string;

use directories::BaseDirs;
use mlua::{Lua, Table};

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::{env, fs};

/// Resolved Nauvim directories in one host environment.
#[derive(Debug, Clone)]
pub struct ProjectDirs {
    pub cache_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

/// Complete information about the client or selected host environment.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub host_name: String,
    pub user: String,
    pub config_file: Option<PathBuf>,
    pub home_dir: PathBuf,
    pub project_dirs: ProjectDirs,
}

impl HostInfo {
    /// Acquires local information; `config_file` distinguishes the client role.
    pub fn local(config_file: Option<&Path>) -> Result<Self, Error> {
        let dirs = BaseDirs::new()
            .ok_or_else(|| Error::Message("Cannot determine home directory".into()))?;
        let (host_name, user) = local_identity()?;
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs.runtime_dir().map(Path::to_path_buf))
            .unwrap_or_else(|| {
                env::temp_dir().join(format!("nauvim-runtime-{}", stable_hash(user.bytes())))
            });
        let cache = env::var_os("XDG_CACHE_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs.cache_dir().to_path_buf());
        Ok(Self {
            host_name,
            user,
            config_file: config_file.map(fs::canonicalize).transpose()?,
            home_dir: resolve_path(dirs.home_dir())?,
            project_dirs: ProjectDirs {
                cache_dir: resolve_path(&cache.join("nauvim"))?,
                data_dir: project_data_dir(&dirs)?,
                runtime_dir: resolve_path(&runtime.join("nauvim"))?,
            },
        })
    }

    pub(crate) fn resolved(mut self) -> Result<Self, Error> {
        self.config_file = self.config_file.as_deref().map(resolve_path).transpose()?;
        self.home_dir = resolve_path(&self.home_dir)?;
        self.project_dirs.cache_dir = resolve_path(&self.project_dirs.cache_dir)?;
        self.project_dirs.data_dir = resolve_path(&self.project_dirs.data_dir)?;
        self.project_dirs.runtime_dir = resolve_path(&self.project_dirs.runtime_dir)?;
        Ok(self)
    }

    pub(crate) fn table(&self, lua: &Lua) -> Result<Table, Error> {
        fn text<'a>(path: &'a Path, field: &str) -> Result<&'a str, Error> {
            let path = path
                .to_str()
                .ok_or_else(|| Error::Message(format!("{field} must be valid UTF-8")))?;
            validate_string(path, field)?;
            Ok(path)
        }
        validate_string(&self.host_name, "HostInfo.host_name")?;
        validate_string(&self.user, "HostInfo.user")?;
        let t = lua.create_table()?;
        t.set("host_name", self.host_name.as_str())?;
        t.set("user", self.user.as_str())?;
        t.set(
            "config_file",
            self.config_file
                .as_deref()
                .map(|p| text(p, "HostInfo.config_file"))
                .transpose()?,
        )?;
        t.set("home_dir", text(&self.home_dir, "HostInfo.home_dir")?)?;
        let dirs = lua.create_table()?;
        dirs.set(
            "cache_dir",
            text(
                &self.project_dirs.cache_dir,
                "HostInfo.project_dirs.cache_dir",
            )?,
        )?;
        dirs.set(
            "data_dir",
            text(
                &self.project_dirs.data_dir,
                "HostInfo.project_dirs.data_dir",
            )?,
        )?;
        dirs.set(
            "runtime_dir",
            text(
                &self.project_dirs.runtime_dir,
                "HostInfo.project_dirs.runtime_dir",
            )?,
        )?;
        t.set("project_dirs", dirs)?;
        Ok(t)
    }
}

pub(crate) fn local_data_dir() -> Result<PathBuf, Error> {
    let dirs =
        BaseDirs::new().ok_or_else(|| Error::Message("Cannot determine home directory".into()))?;
    project_data_dir(&dirs)
}

fn project_data_dir(dirs: &BaseDirs) -> Result<PathBuf, Error> {
    let data_dir = env::var_os("XDG_DATA_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs.data_dir().to_path_buf());
    resolve_path(&data_dir.join("nauvim"))
}

pub(crate) fn local_identity() -> Result<(String, String), Error> {
    let user = output("id", &["-un"])?;
    let host_name = output("hostname", &[])?;
    Ok((host_name, user))
}

fn output(prog: &str, args: &[&str]) -> Result<String, Error> {
    use std::process::Command;

    let out = Command::new(prog).args(args).output()?;
    if !out.status.success() {
        return Err(Error::Message(format!(
            "Cannot acquire host information using {prog}"
        )));
    }
    let ret = String::from_utf8(out.stdout)?.trim().to_owned();
    validate_string(&ret, "Host information")?;
    if ret.is_empty() {
        return Err(Error::Message(format!(
            "{prog} returned empty host information"
        )));
    }
    Ok(ret)
}

pub(crate) fn resolve_path(path: &Path) -> Result<PathBuf, Error> {
    use std::io::ErrorKind;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut pending: VecDeque<OsString> = absolute
        .components()
        .filter_map(|c| match c {
            Component::RootDir | Component::CurDir => None,
            c => Some(c.as_os_str().to_owned()),
        })
        .collect();
    let mut ret = PathBuf::from("/");
    let mut links = 0_u8;
    while let Some(c) = pending.pop_front() {
        if c == ".." {
            ret.pop();
            continue;
        }
        if c == "." {
            continue;
        }
        ret.push(c);
        match fs::symlink_metadata(&ret) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if links >= 40 {
                    return Err(Error::Message(format!(
                        "Too many symbolic links while resolving {}",
                        path.display()
                    )));
                }
                links = links.saturating_add(1);
                let target = fs::read_link(&ret)?;
                ret.pop();
                if target.is_absolute() {
                    ret = PathBuf::from("/");
                }
                for c in target.components().rev() {
                    match c {
                        Component::RootDir | Component::CurDir => {}
                        c => pending.push_front(c.as_os_str().to_owned()),
                    }
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(ret)
}

pub(crate) fn stable_hash(bytes: impl Iterator<Item = u8>) -> u64 {
    bytes.fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use std::os::unix::fs::symlink;

    #[test]
    fn resolves_symlink_targets_before_parent_components() {
        let dir = tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("real/child")).unwrap();
        symlink("real/child", root.join("link")).unwrap();
        assert_eq!(
            resolve_path(&root.join("link/../cache")).unwrap(),
            root.join("real/cache")
        );
        assert!(!root.join("real/cache").exists());
    }

    #[test]
    fn resolves_dangling_links_without_creating_directories() {
        let dir = tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        symlink("missing/child", root.join("link")).unwrap();
        assert_eq!(
            resolve_path(&root.join("link/../cache")).unwrap(),
            root.join("missing/cache")
        );
        assert!(!root.join("missing").exists());
        symlink("loop", root.join("loop")).unwrap();
        assert!(resolve_path(&root.join("loop/cache")).is_err());
    }
}
