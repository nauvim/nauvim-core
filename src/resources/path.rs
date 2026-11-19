use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// Resolves links and relative components while retaining missing descendants.
///
/// Components are processed in filesystem order so that `link/..` refers to
/// the link target's parent. Missing suffixes are needed for copy destinations.
pub(super) fn resolve(path: &Path) -> io::Result<PathBuf> {
    use std::env::current_dir;

    let mut ret = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        current_dir()?
    };
    let mut pending: VecDeque<_> = components(path).map(Cow::Borrowed).collect();
    let mut links = 0_u8;
    let mut directory = true;
    while let Some(name) = pending.pop_front() {
        let name: &OsStr = name.as_ref();
        if !directory {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "Path component is not a directory",
            ));
        }
        if name == "." {
            continue;
        }
        if name == ".." {
            ret.pop();
            continue;
        }
        ret.push(name);
        match fs::symlink_metadata(&ret) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                links = links.checked_add(1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Too many symbolic links")
                })?;
                if links > 40 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Too many symbolic links",
                    ));
                }
                let target = fs::read_link(&ret)?;
                ret.pop();
                directory = true;
                if target.is_absolute() {
                    ret = PathBuf::from("/");
                }
                for name in components(&target).rev() {
                    pending.push_front(Cow::Owned(name.to_owned()));
                }
            }
            Ok(metadata) => directory = metadata.is_dir(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => directory = true,
            Err(e) => return Err(e),
        }
    }
    Ok(ret)
}

fn components(path: &Path) -> impl DoubleEndedIterator<Item = &OsStr> {
    let bytes = path.as_os_str().as_bytes();
    bytes
        .split(|byte| *byte == b'/')
        .filter(|name| !name.is_empty())
        .map(OsStr::from_bytes)
        .chain((bytes.last() == Some(&b'/')).then_some(OsStr::new(".")))
}

pub(super) fn descendant(base: &Path, path: &Path) -> io::Result<PathBuf> {
    let base = fs::canonicalize(base)?;
    let path = resolve(path)?;
    if path == base || !path.starts_with(base) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Client path must be a strict descendant of the process temporary directory",
        ));
    }
    Ok(path)
}

pub(super) fn valid_string(value: &str) -> Result<(), String> {
    if value.contains(['\0', '\u{feff}']) {
        Err("Plugin strings must not contain NUL or a byte order mark".to_owned())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use std::os::unix::fs::symlink;

    #[test]
    fn resolves_links_before_parent_components_and_missing_suffixes() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("base");
        fs::create_dir_all(base.join("nested/deeper")).unwrap();
        symlink("nested/deeper", base.join("link")).unwrap();
        assert_eq!(
            descendant(&base, &base.join("link/../new/file")).unwrap(),
            base.join("nested/new/file")
        );
        assert_eq!(
            descendant(&base, &base.join("missing/../new")).unwrap(),
            base.join("new")
        );
        for path in ["", ".", "nested/..", "../outside"] {
            assert!(descendant(&base, &base.join(path)).is_err(), "{path}");
        }
        symlink("../outside/missing", base.join("escape")).unwrap();
        assert!(descendant(&base, &base.join("escape")).is_err());
    }

    #[test]
    fn rejects_file_ancestors_and_link_cycles() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file"), b"x").unwrap();
        for path in ["file/..", "file/.", "file/child", "file/"] {
            assert_eq!(
                resolve(&dir.path().join(path)).unwrap_err().kind(),
                io::ErrorKind::NotADirectory,
                "{path}"
            );
        }
        symlink("b", dir.path().join("a")).unwrap();
        symlink("a", dir.path().join("b")).unwrap();
        assert_eq!(
            resolve(&dir.path().join("a")).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
