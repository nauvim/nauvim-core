use nauvim_core::{Config, HostInfo};

use std::fs;
use std::path::Path;

pub mod checkpoint;

pub fn isolated_config(root: &Path, src: &str, ui_id: &str) -> Config {
    let file = root.join("config.lua");
    fs::write(&file, src).unwrap();
    let mut info = HostInfo::local(Some(&file)).unwrap();
    info.project_dirs.data_dir = root.join("data");
    Config::evaluate(src, info, ui_id, |_| Ok(())).unwrap().0
}

pub fn is_clipboard_detection_error(error: &str) -> bool {
    cfg!(feature = "plugin-clipboard") && error.starts_with("Detect clipboard backend: ")
}
