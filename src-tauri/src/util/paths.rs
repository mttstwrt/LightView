use std::path::PathBuf;

/// Return the portable `data/` directory next to the running executable.
///
/// Layout:
///   <exe_dir>/data/plugins/
///   <exe_dir>/data/recent.json
pub fn data_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("failed to locate running executable");
    exe.parent()
        .expect("executable has no parent directory")
        .join("data")
}
