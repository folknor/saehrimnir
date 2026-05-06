use std::io;
use std::path::Path;

use tokio::fs;

/// One line of the readiness sentinel: `<name> <port>`.
#[derive(Debug, Clone, Copy)]
pub struct ProtocolPort {
    /// Wire name as it appears in the sentinel. JMAP is historically
    /// `READY` for back-compat; everything else is the protocol name.
    pub name: &'static str,
    pub port: u16,
}

/// Atomically write the readiness sentinel to `path`.
///
/// Format: one `<name> <port>\n` line per entry. Writes to a sibling
/// temp file in the same directory, then renames into place. Same-
/// filesystem rename is atomic on POSIX.
pub async fn write_ready(path: &Path, ports: &[ProtocolPort]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "readiness path has no filename")
    })?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".tmp");
    let tmp_path = parent.join(tmp_name);

    let mut body = String::new();
    for p in ports {
        body.push_str(&format!("{} {}\n", p.name, p.port));
    }
    fs::write(&tmp_path, body).await?;
    fs::rename(&tmp_path, path).await?;
    Ok(())
}
