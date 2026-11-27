//! Start at logon: a Task Scheduler task runs the copy in Program Files with
//! highest privileges, so the tray starts elevated without a UAC prompt.
//! The copy must live where only admins can write: anything that can replace
//! the exe would otherwise get silent elevation at the next logon.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, HLOCAL, HWND, LPARAM,
    LocalFree, WPARAM,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_ELEVATION, TOKEN_QUERY,
    TOKEN_USER, TokenElevation, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_SHARE_READ, GetFileAttributesW,
    GetFinalPathNameByHandleW, INVALID_FILE_ATTRIBUTES, MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_MULTI_SZ, RRF_RT_REG_MULTI_SZ, RegCloseKey,
    RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

const TASK_NAME: &str = "victus-control";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Protected DACL: SYSTEM and Administrators full control, Users read & execute.
const INSTALL_DIR_SDDL: PCWSTR = w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)");
const SESSION_MANAGER_KEY: PCWSTR = w!("SYSTEM\\CurrentControlSet\\Control\\Session Manager");
const PENDING_RENAMES_VALUE: PCWSTR = w!("PendingFileRenameOperations");

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct OwnedLocal(HLOCAL);

impl Drop for OwnedLocal {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(self.0));
            }
        }
    }
}

struct OwnedKey(HKEY);

impl Drop for OwnedKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

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
    let me = current_exe_live().or_else(|_| std::env::current_exe().map_err(|e| e.to_string()));
    match (me, installed_exe()) {
        (Ok(me), Ok(installed)) => same_path(&me, &installed),
        _ => false,
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let resolved = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        normalize_path(&resolved)
    };
    norm(a) == norm(b)
}

fn normalize_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\").or_else(|| s.strip_prefix(r"\\?\unc\")) {
        format!(r"\\{}", rest.to_ascii_lowercase())
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_ascii_lowercase()
    }
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

/// Current process's live kernel image path (follows renames, unlike `GetModuleFileNameW`).
fn current_exe_live() -> Result<PathBuf, String> {
    process_image_path(unsafe { GetCurrentProcess() })
}

fn process_image_path(process: HANDLE) -> Result<PathBuf, String> {
    let mut buf = vec![0u16; 512];
    loop {
        let mut len = buf.len() as u32;
        match unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) } {
            Ok(()) => return Ok(PathBuf::from(OsString::from_wide(&buf[..len as usize]))),
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() && buf.len() < 32_768 => {
                buf.resize(buf.len() * 2, 0);
            }
            Err(e) => return Err(format!("can't query process image path: {e}")),
        }
    }
}

fn final_path_of_file(file: &File) -> Result<PathBuf, String> {
    let handle = HANDLE(file.as_raw_handle());
    let mut buf = vec![0u16; 512];
    loop {
        let len = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED) } as usize;
        if len == 0 {
            return Err(format!("can't resolve opened file path: {}", windows::core::Error::from_thread()));
        }
        if len < buf.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buf[..len])));
        }
        buf.resize(len + 1, 0);
    }
}

/// Opens the running executable with read-only sharing (blocking concurrent
/// writes or renames), verifies the opened handle still matches the live kernel
/// image path, and reads its bytes.
fn read_self_image() -> Result<Vec<u8>, String> {
    let live_before = current_exe_live()?;
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(&live_before)
        .map_err(|e| format!("can't open {}: {e}", live_before.display()))?;
    let opened = final_path_of_file(&file)?;
    let live_after = current_exe_live()?;
    if normalize_path(&opened) != normalize_path(&live_after) {
        return Err("running executable changed on disk during install".into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| format!("can't read {}: {e}", live_before.display()))?;
    Ok(bytes)
}

/// Current user's SID string (`S-1-5-...`) from this process's token.
fn current_user_sid() -> Result<String, String> {
    unsafe {
        let mut raw_token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token)
            .map_err(|e| format!("can't open process token: {e}"))?;
        let token = OwnedHandle(raw_token);

        let mut needed = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return Err(format!("can't query token user size: {}", windows::core::Error::from_thread()));
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * size_of::<u64>()) as u32,
            &mut needed,
        )
        .map_err(|e| format!("can't read token user: {e}"))?;

        let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
        let mut wide = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut wide)
            .map_err(|e| format!("can't format user SID: {e}"))?;
        let _guard = OwnedLocal(HLOCAL(wide.0.cast()));
        Ok(String::from_utf16_lossy(wide.as_wide()))
    }
}

fn is_reparse_point(path: &Path) -> bool {
    let attrs = unsafe { GetFileAttributesW(&HSTRING::from(path)) };
    attrs != INVALID_FILE_ATTRIBUTES && (attrs & FILE_ATTRIBUTE_REPARSE_POINT.0) != 0
}

fn ensure_safe_install_dir(dir: &Path) -> Result<(), String> {
    if is_reparse_point(dir) {
        return Err(format!("{} is a link/reparse point; refusing to use it", dir.display()));
    }
    fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    if is_reparse_point(dir) {
        return Err(format!("{} is a link/reparse point; refusing to use it", dir.display()));
    }
    apply_install_dir_dacl(dir)
}

fn apply_install_dir_dacl(dir: &Path) -> Result<(), String> {
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(INSTALL_DIR_SDDL, SDDL_REVISION_1, &mut sd, None)
            .map_err(|e| format!("can't build install folder ACL: {e}"))?;
        let _guard = OwnedLocal(HLOCAL(sd.0));

        let mut present = BOOL(0);
        let mut defaulted = BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted)
            .map_err(|e| format!("can't extract install folder DACL: {e}"))?;
        if !present.as_bool() || dacl.is_null() {
            return Err("install folder DACL is missing".into());
        }

        let rc = SetNamedSecurityInfoW(
            &HSTRING::from(dir),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        );
        if rc != ERROR_SUCCESS {
            return Err(format!("can't lock down {}: {}", dir.display(), windows::core::Error::from(rc)));
        }
        Ok(())
    }
}

fn schtasks(args: &[&str]) -> Result<(), String> {
    let sys = system_dir()?;
    let out = Command::new(sys.join("schtasks.exe"))
        .args(args)
        .current_dir(&sys)
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
    let user = current_user_sid()?;
    // Written next to the exe, not to %TEMP%: a non-elevated process could
    // change a file in %TEMP% between writing and schtasks reading it.
    let xml_path = install_dir()?.join("task.xml");
    if is_reparse_point(&xml_path) {
        return Err(format!("{} is a link/reparse point; refusing to write it", xml_path.display()));
    }
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
    let self_bytes = if running_installed() { None } else { Some(read_self_image()?) };
    close_running_tray();
    let dir = install_dir()?;
    ensure_safe_install_dir(&dir)?;
    clear_pending_deletes_under(&dir);
    if let Some(bytes) = self_bytes {
        write_with_retry(&bytes, &installed_exe()?)?;
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
    if is_reparse_point(&dir) {
        return Err(format!("{} is a link/reparse point; refusing to delete it", dir.display()));
    }
    if !dir.exists() {
        return Ok(true);
    }
    if fs::remove_dir_all(&dir).is_ok() {
        return Ok(true);
    }
    let mut left = Vec::new();
    collect_bottom_up(&dir, &mut left)?;
    for path in left {
        unsafe { MoveFileExW(&HSTRING::from(path.as_path()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) }
            .map_err(|e| format!("can't remove {}: {e}", path.display()))?;
    }
    Ok(false)
}

fn collect_bottom_up(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("can't list {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("can't read {}: {e}", dir.display()))?;
        let path = entry.path();
        if !is_reparse_point(&path) && path.is_dir() {
            collect_bottom_up(&path, out)?;
        } else {
            out.push(path);
        }
    }
    out.push(dir.to_path_buf());
    Ok(())
}

fn is_pid_elevated(pid: u32) -> bool {
    unsafe {
        let Ok(raw_proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let proc = OwnedHandle(raw_proc);
        let mut raw_token = HANDLE::default();
        if OpenProcessToken(proc.0, TOKEN_QUERY, &mut raw_token).is_err() {
            return false;
        }
        let token = OwnedHandle(raw_token);
        let mut elev = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elev as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok()
            && elev.TokenIsElevated != 0
    }
}

/// Finds top-level `"victus-control"` windows belonging to other elevated processes.
fn find_elevated_tray_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let mut class = [0u16; 32];
        let len = unsafe { GetClassNameW(hwnd, &mut class) };
        let expected = w!("victus-control");
        if len > 0 && &class[..len as usize] == unsafe { expected.as_wide() } {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            if pid != 0 && pid != std::process::id() && is_pid_elevated(pid) {
                unsafe { (&mut *(lparam.0 as *mut Vec<HWND>)).push(hwnd) };
            }
        }
        BOOL(1)
    }

    let mut windows = Vec::new();
    let ptr = (&mut windows as *mut Vec<HWND>) as isize;
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(ptr));
    }
    windows
}

fn close_running_tray() {
    let targets = find_elevated_tray_windows();
    if targets.is_empty() {
        return;
    }
    for hwnd in targets {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    let start = Instant::now();
    while !find_elevated_tray_windows().is_empty() && start.elapsed() < Duration::from_secs(5) {
        sleep(Duration::from_millis(100));
    }
}

/// The closed tray's process may still hold the exe for a moment after its
/// window is gone.
fn write_with_retry(bytes: &[u8], to: &Path) -> Result<(), String> {
    if is_reparse_point(to) {
        return Err(format!("{} is a link/reparse point; refusing to overwrite it", to.display()));
    }
    let start = Instant::now();
    loop {
        match fs::write(to, bytes) {
            Ok(()) => return Ok(()),
            Err(e) if start.elapsed() > Duration::from_secs(5) => {
                return Err(format!("can't write {}: {e}", to.display()));
            }
            Err(_) => sleep(Duration::from_millis(200)),
        }
    }
}

/// Removes any queued reboot-time deletes inside `dir` left over from a prior
/// `--uninstall` so reinstalling before reboot isn't undone on restart.
fn clear_pending_deletes_under(dir: &Path) {
    unsafe {
        let mut raw_key = HKEY::default();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, SESSION_MANAGER_KEY, None, KEY_READ | KEY_WRITE, &mut raw_key)
            != ERROR_SUCCESS
        {
            return;
        }
        let key = OwnedKey(raw_key);

        let mut byte_len = 0u32;
        let rc = RegGetValueW(
            key.0,
            PCWSTR::null(),
            PENDING_RENAMES_VALUE,
            RRF_RT_REG_MULTI_SZ,
            None,
            None,
            Some(&mut byte_len),
        );
        if rc != ERROR_SUCCESS || byte_len < 2 {
            return;
        }
        let mut wide = vec![0u16; (byte_len as usize).div_ceil(2)];
        if RegGetValueW(
            key.0,
            PCWSTR::null(),
            PENDING_RENAMES_VALUE,
            RRF_RT_REG_MULTI_SZ,
            None,
            Some(wide.as_mut_ptr().cast()),
            Some(&mut byte_len),
        ) != ERROR_SUCCESS
        {
            return;
        }
        wide.truncate((byte_len as usize) / 2);

        let entries = decode_multi_sz(&wide);
        let pruned = prune_pending_deletes(&entries, dir);
        if pruned.len() == entries.len() {
            return;
        }
        if pruned.is_empty() {
            let _ = RegDeleteValueW(key.0, PENDING_RENAMES_VALUE);
        } else {
            let encoded = encode_multi_sz(&pruned);
            let bytes = std::slice::from_raw_parts(encoded.as_ptr().cast::<u8>(), encoded.len() * 2);
            let _ = RegSetValueExW(key.0, PENDING_RENAMES_VALUE, None, REG_MULTI_SZ, Some(bytes));
        }
    }
}

fn decode_multi_sz(wide: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < wide.len() {
        // An empty source entry at an even index terminates the pair list;
        // an empty destination entry at an odd index marks a pending delete.
        if out.len().is_multiple_of(2) && wide[start] == 0 {
            break;
        }
        let end = wide[start..].iter().position(|&c| c == 0).map_or(wide.len(), |i| start + i);
        out.push(String::from_utf16_lossy(&wide[start..end]));
        start = end + 1;
    }
    out
}

fn encode_multi_sz(entries: &[String]) -> Vec<u16> {
    let mut out = Vec::new();
    for s in entries {
        out.extend(s.encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

fn path_is_or_under(candidate: &str, dir: &Path) -> bool {
    let cand = candidate.strip_prefix(r"\??\").unwrap_or(candidate).to_ascii_lowercase();
    let base = normalize_path(dir);
    let base = base.trim_end_matches('\\');
    cand == base || cand.strip_prefix(base).is_some_and(|rest| rest.starts_with('\\'))
}

fn prune_pending_deletes(entries: &[String], dir: &Path) -> Vec<String> {
    let mut out = Vec::with_capacity(entries.len());
    let (pairs, remainder) = entries.as_chunks::<2>();
    for [src, dst] in pairs {
        if dst.is_empty() && path_is_or_under(src, dir) {
            continue;
        }
        out.push(src.clone());
        out.push(dst.clone());
    }
    out.extend_from_slice(remainder);
    out
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
    fn paths_compare_case_insensitively_and_strip_extended_prefix() {
        assert!(same_path(Path::new(r"C:\Program Files\X.exe"), Path::new(r"c:\program files\x.EXE")));
        assert!(same_path(Path::new(r"\\?\C:\Program Files\X.exe"), Path::new(r"c:\program files\x.exe")));
        assert!(same_path(
            Path::new(r"\\?\UNC\server\share\victus-control.exe"),
            Path::new(r"\\SERVER\share\victus-control.exe"),
        ));
        assert!(!same_path(Path::new(r"C:\a.exe"), Path::new(r"C:\b.exe")));
    }

    #[test]
    fn current_user_sid_starts_with_s_1() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "unexpected SID: {sid}");
    }

    #[test]
    fn read_self_image_reads_pe_header() {
        let bytes = read_self_image().unwrap();
        assert!(bytes.starts_with(b"MZ"), "expected PE binary");
    }

    #[test]
    fn decode_multi_sz_handles_empty_short_delete_and_rename_pairs() {
        assert_eq!(decode_multi_sz(&[0, 0]), Vec::<String>::new());
        assert_eq!(decode_multi_sz(&[b'a' as u16, 0, 0]), vec!["a", ""]);
        assert_eq!(decode_multi_sz(&[b'a' as u16, 0, 0, 0]), vec!["a", ""]);
        assert_eq!(
            decode_multi_sz(&[b'a' as u16, 0, b'b' as u16, 0, 0]),
            vec!["a", "b"]
        );
    }

    #[test]
    fn collect_bottom_up_orders_children_before_parents() {
        let root = std::env::temp_dir().join(format!("victus-control-test-{}", std::process::id()));
        let sub = root.join("sub");
        let file = sub.join("nested.txt");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&sub).unwrap();
        fs::write(&file, b"ok").unwrap();

        let mut out = Vec::new();
        collect_bottom_up(&root, &mut out).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(out, vec![file, sub, root]);
    }

    #[test]
    fn prune_pending_deletes_drops_only_target_dir_deletes() {
        let dir = Path::new(r"C:\Program Files\victus-control");
        let entries = vec![
            r"\??\C:\Program Files\victus-control\victus-control.exe".into(),
            String::new(),
            r"\??\C:\Program Files\victus-control".into(),
            String::new(),
            r"\??\C:\Program Files\victus-control-other\app.exe".into(),
            String::new(),
            r"\??\C:\Program Files\victus-control\renamed.exe".into(),
            r"\??\C:\Temp\kept.exe".into(),
            r"\??\C:\Other\old.dll".into(),
            String::new(),
        ];
        let encoded = encode_multi_sz(&entries);
        assert_eq!(decode_multi_sz(&encoded), entries);

        let pruned = prune_pending_deletes(&entries, dir);
        assert_eq!(
            pruned,
            vec![
                r"\??\C:\Program Files\victus-control-other\app.exe",
                "",
                r"\??\C:\Program Files\victus-control\renamed.exe",
                r"\??\C:\Temp\kept.exe",
                r"\??\C:\Other\old.dll",
                "",
            ]
        );
    }
}
