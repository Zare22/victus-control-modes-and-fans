//! Start at logon: a Task Scheduler task runs the copy in Program Files with
//! highest privileges, so the tray starts elevated without a UAC prompt.
//! The copy must live where only admins can write: anything that can replace
//! the exe would otherwise get silent elevation at the next logon.

use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};
use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

const TASK_NAME: &str = "victus-control";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Also holds the setting and the log. Not taken from `%ProgramFiles%`:
/// non-elevated code can change the environment an elevated process starts with.
pub fn install_dir() -> Result<PathBuf, String> {
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None)
            .map_err(|e| format!("can't find Program Files: {e}"))?;
        let dir = PathBuf::from(OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.0 as *const _));
        Ok(dir.join("victus-control"))
    }
}

pub fn installed_exe() -> Result<PathBuf, String> {
    Ok(install_dir()?.join("victus-control.exe"))
}

pub fn running_installed() -> bool {
    match (std::env::current_exe(), installed_exe()) {
        (Ok(me), Ok(installed)) => same_path(&me, &installed),
        _ => false,
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().to_string_lossy().eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

/// A bare `schtasks.exe` would be looked up in this exe's folder before System32.
fn system_dir() -> Result<PathBuf, String> {
    let mut buf = [0u16; 260];
    let len = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if len == 0 || len >= buf.len() {
        return Err("can't find the System32 folder".into());
    }
    Ok(PathBuf::from(OsString::from_wide(&buf[..len])))
}

/// `DOMAIN\name` from this process's token, not from `%USERDOMAIN%` / `%USERNAME%`.
fn current_user() -> Result<String, String> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    if !unsafe { GetUserNameExW(NameSamCompatible, Some(PWSTR(buf.as_mut_ptr())), &mut len) } {
        return Err(format!("can't tell the current user: {}", windows::core::Error::from_thread()));
    }
    Ok(String::from_utf16_lossy(&buf[..len as usize]))
}

fn schtasks(args: &[&str]) -> Result<(), String> {
    let out = Command::new(system_dir()?.join("schtasks.exe"))
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("can't run schtasks: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("schtasks {} failed: {}", args[0], String::from_utf8_lossy(&out.stderr).trim()))
    }
}

pub fn is_enabled() -> bool {
    schtasks(&["/query", "/tn", TASK_NAME]).is_ok()
}

/// Registers (or replaces) the logon task for the installed exe.
pub fn enable() -> Result<(), String> {
    let user = current_user()?;
    // Written next to the exe, not to %TEMP%: a non-elevated process could
    // change a file in %TEMP% between writing and schtasks reading it.
    let xml_path = install_dir()?.join("task.xml");
    let mut bytes = vec![0xFF, 0xFE];
    for unit in task_xml(&installed_exe()?, &user).encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(&xml_path, bytes).map_err(|e| format!("can't write {}: {e}", xml_path.display()))?;
    let result = schtasks(&["/create", "/tn", TASK_NAME, "/xml", &xml_path.to_string_lossy(), "/f"]);
    let _ = fs::remove_file(&xml_path);
    result
}

pub fn disable() -> Result<(), String> {
    schtasks(&["/delete", "/tn", TASK_NAME, "/f"])
}

/// Copies this exe to Program Files, registers the logon task and starts the
/// installed tray (closing any running one first).
pub fn install() -> Result<(), String> {
    close_running_tray();
    if !running_installed() {
        let me = std::env::current_exe().map_err(|e| format!("can't find this exe: {e}"))?;
        let dir = install_dir()?;
        fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        copy_with_retry(&me, &installed_exe()?)?;
    }
    enable()?;
    schtasks(&["/run", "/tn", TASK_NAME])
}

/// Returns `false` when files are still in use and will go at the next restart.
pub fn uninstall() -> Result<bool, String> {
    close_running_tray();
    if is_enabled() {
        disable()?;
    }
    let dir = install_dir()?;
    if !dir.exists() || fs::remove_dir_all(&dir).is_ok() {
        return Ok(true);
    }
    let left = fs::read_dir(&dir).map_err(|e| format!("can't list {}: {e}", dir.display()))?;
    for path in left.filter_map(|entry| entry.ok().map(|e| e.path())).chain([dir.clone()]) {
        unsafe { MoveFileExW(&HSTRING::from(path.as_path()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) }
            .map_err(|e| format!("can't remove {}: {e}", path.display()))?;
    }
    Ok(false)
}

fn close_running_tray() {
    let find = || unsafe { FindWindowW(w!("victus-control"), PCWSTR::null()) };
    let Ok(hwnd) = find() else { return };
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let start = Instant::now();
    while find().is_ok() && start.elapsed() < Duration::from_secs(5) {
        sleep(Duration::from_millis(100));
    }
}

/// The closed tray's process may still hold the exe for a moment after its
/// window is gone.
fn copy_with_retry(from: &Path, to: &Path) -> Result<(), String> {
    let start = Instant::now();
    loop {
        match fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(e) if start.elapsed() > Duration::from_secs(5) => {
                return Err(format!("can't copy to {}: {e}", to.display()));
            }
            Err(_) => sleep(Duration::from_millis(200)),
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Battery and time limits are off: Task Scheduler's defaults would stop the
/// tray on unplug, not start it on battery, and kill it after 3 days.
fn task_xml(exe: &Path, user: &str) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let user = xml_escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts the victus-control tray icon at logon.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{exe}"</Command>
    </Exec>
  </Actions>
</Task>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_xml_has_path_user_and_no_battery_or_time_limits() {
        let xml = task_xml(Path::new(r"C:\Program Files\victus-control\victus-control.exe"), "PC\\A&B");
        assert!(xml.contains(r#"<Command>"C:\Program Files\victus-control\victus-control.exe"</Command>"#));
        assert!(xml.contains("<UserId>PC\\A&amp;B</UserId>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }

    #[test]
    fn install_dir_ignores_the_program_files_variable() {
        let real = install_dir().unwrap();
        let saved = std::env::var_os("ProgramFiles");
        unsafe { std::env::set_var("ProgramFiles", r"C:\Users\Public\fake") };
        let redirected = install_dir();
        if let Some(value) = saved {
            unsafe { std::env::set_var("ProgramFiles", value) };
        }
        assert_eq!(redirected.unwrap(), real);
        assert!(real.ends_with(r"Program Files\victus-control"), "{}", real.display());
    }

    #[test]
    fn paths_compare_case_insensitively() {
        assert!(same_path(Path::new(r"C:\Program Files\X.exe"), Path::new(r"c:\program files\x.EXE")));
        assert!(!same_path(Path::new(r"C:\a.exe"), Path::new(r"C:\b.exe")));
    }
}
