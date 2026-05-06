use std::io;
use std::path::Path;

use tokio::fs;

/// Atomically write `READY <port>\n` to `path` so a watcher can never
/// observe a half-written file.
///
/// Writes to a sibling temp file in the same directory, then renames into
/// place. Same-filesystem rename is atomic on POSIX.
pub async fn write_ready(path: &Path, port: u16) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "readiness path has no filename")
    })?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".tmp");
    let tmp_path = parent.join(tmp_name);

    fs::write(&tmp_path, format!("READY {port}\n")).await?;
    fs::rename(&tmp_path, path).await?;
    Ok(())
}
