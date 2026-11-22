pub mod checkpoint;

pub fn is_clipboard_detection_error(error: &str) -> bool {
    cfg!(feature = "plugin-clipboard") && error.starts_with("Detect clipboard backend: ")
}
