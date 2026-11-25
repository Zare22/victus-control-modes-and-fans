//! Local-only log, `log.txt` in the install folder (admin-only, like the
//! setting in `config`). When it passes `MAX_BYTES` it is moved to
//! `log.old.txt`, so at most two files exist.

use std::fs::{self, OpenOptions};
use std::io::Write;

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::autostart::install_dir;

const MAX_BYTES: u64 = 256 * 1024;

/// Failures are ignored: the log must never stop the app.
pub fn write(message: &str) {
    let Ok(dir) = install_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("log.txt");
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(&path, dir.join("log.old.txt"));
    }
    let t = unsafe { GetLocalTime() };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {message}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        );
    }
}
