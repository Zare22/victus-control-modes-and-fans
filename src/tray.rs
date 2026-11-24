use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    GetSystemPowerStatus, POWERBROADCAST_SETTING, RegisterPowerSettingNotification, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::SystemServices::{GUID_ACDC_POWER_SOURCE, GUID_CONSOLE_DISPLAY_STATE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFY_ICON_INFOTIP_FLAGS,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreatePopupMenu, CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE,
    DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW, GetMessageW, HICON, HMENU,
    KillTimer, MF_BYCOMMAND, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_POWERBROADCAST, WM_TIMER, WNDCLASSW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::autostart;
use crate::config::Config;
use crate::icon;
use crate::log;
use crate::victus::{self, MaxFan, Mode, State, Victus};

const WM_TRAY: u32 = WM_APP + 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const TRAY_ID: u32 = 1;

const VERIFY_TIMER: usize = 1;
const VERIFY_POLL_MS: u32 = 250;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// Re-apply waits a little after resume / plug-in so the BIOS has settled.
const REAPPLY_TIMER: usize = 2;
const REAPPLY_DELAY_MS: u32 = 2000;

const ID_MODE_FIRST: u32 = 100;
const ID_MAX_FAN: u32 = 200;
const ID_AUTOSTART: u32 = 250;
const ID_EXIT: u32 = 300;

/// Explorer broadcasts this after it restarts; the icon has to be added again.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Returns `None` instead of panicking when called re-entrantly (e.g. from a
/// message dispatched inside a modal menu loop while `APP` is borrowed).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut app = cell.try_borrow_mut().ok()?;
        app.as_mut().map(f)
    })
}

struct App {
    victus: Victus,
    hwnd: HWND,
    icon: HICON,
    tip: String,
    verifying: Option<(Mode, Instant)>,
    reapply_reason: Option<&'static str>,
    on_ac: Option<bool>,
    display_on: Option<bool>,
    autostart: bool,
}

/// Returns once the user picks Exit. A second instance returns immediately.
pub fn run(force: bool) -> Result<(), String> {
    unsafe {
        let _single_instance = CreateMutexW(None, false, w!("Local\\victus-control-tray"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        let victus = Victus::open(force).map_err(|e| e.to_string())?;
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = w!("victus-control");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("RegisterClassW failed".into());
        }
        // A hidden top-level window rather than a message-only one: those
        // don't receive broadcasts such as TaskbarCreated.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            class,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let icon = icon::create().map_err(|e| e.to_string())?;
        let mut app = App {
            victus,
            hwnd,
            icon,
            tip: String::new(),
            verifying: None,
            reapply_reason: None,
            on_ac: on_ac_power(),
            display_on: None,
            autostart: autostart::is_enabled(),
        };
        app.tip = tooltip(&app.victus.read_state());
        app.add_icon();
        app.reapply("start");
        APP.with(|cell| *cell.borrow_mut() = Some(app));

        // This laptop only has Modern Standby, which usually sends no resume
        // event; the display coming back on is the reliable wake signal.
        for (guid, what) in [(GUID_ACDC_POWER_SOURCE, "power source"), (GUID_CONSOLE_DISPLAY_STATE, "display state")] {
            if let Err(e) = RegisterPowerSettingNotification(HANDLE(hwnd.0), &guid, DEVICE_NOTIFY_WINDOW_HANDLE) {
                log::write(&format!("can't watch the {what}: {e}"));
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        APP.with(|cell| cell.borrow_mut().take());
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = (lparam.0 & 0xFFFF) as u32;
            if matches!(event, WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT) {
                let x = (wparam.0 & 0xFFFF) as i16 as i32;
                let y = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                show_menu(hwnd, x, y);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == VERIFY_TIMER => {
            with_app(App::check_mode_applied);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == REAPPLY_TIMER => {
            with_app(App::run_scheduled_reapply);
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            match wparam.0 as u32 {
                PBT_APMSUSPEND => log::write("suspend"),
                PBT_APMRESUMEAUTOMATIC => {
                    with_app(|app| app.schedule_reapply("resume"));
                }
                PBT_POWERSETTINGCHANGE => {
                    let setting = unsafe { &*(lparam.0 as *const POWERBROADCAST_SETTING) };
                    if setting.DataLength >= 4 {
                        let value = unsafe { std::ptr::read_unaligned(setting.Data.as_ptr().cast::<u32>()) };
                        if setting.PowerSetting == GUID_ACDC_POWER_SOURCE {
                            // 0 = AC, 1 = battery, 2 = short-term source (UPS).
                            with_app(|app| app.power_source_changed(value == 0));
                        } else if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE {
                            // 0 = off, 1 = on, 2 = dimmed.
                            with_app(|app| app.display_changed(value != 0));
                        }
                    }
                }
                _ => {}
            }
            LRESULT(1)
        }
        WM_DESTROY => {
            with_app(|app| app.remove_icon());
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            with_app(|app| app.add_icon());
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn show_menu(hwnd: HWND, x: i32, y: i32) {
    let Some((state, autostart)) = with_app(|app| {
        let state = app.victus.read_state();
        app.set_tip(tooltip(&state));
        (state, app.autostart)
    }) else {
        return;
    };
    let Ok(menu) = build_menu(&state, autostart) else { return };

    // The menu must not be open while `APP` is borrowed: its modal loop
    // dispatches messages back into `wndproc`.
    let cmd = unsafe {
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, x, y, None, hwnd, None);
        // Without this the menu doesn't close properly on the next click elsewhere.
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as u32
    };

    let current = state.as_ref().ok();
    match cmd {
        ID_EXIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        ID_AUTOSTART => {
            with_app(App::toggle_autostart);
        }
        ID_MAX_FAN => {
            let on = current.is_none_or(|s| s.max_fan != MaxFan::Max);
            with_app(|app| app.choose_max_fan(on));
        }
        id if (ID_MODE_FIRST..ID_MODE_FIRST + Mode::ALL.len() as u32).contains(&id) => {
            let mode = Mode::ALL[(id - ID_MODE_FIRST) as usize];
            if current.and_then(State::mode) != Some(mode) {
                with_app(|app| app.choose_mode(mode));
            }
        }
        _ => {}
    }
}

fn build_menu(state: &Result<State, victus::Error>, autostart: bool) -> windows::core::Result<HMENU> {
    unsafe {
        let menu = CreatePopupMenu()?;
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &HSTRING::from(status_line(state)))?;
        AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        let battery = on_battery();
        for (i, &mode) in Mode::ALL.iter().enumerate() {
            let id = (ID_MODE_FIRST + i as u32) as usize;
            if mode == Mode::Performance && battery {
                AppendMenuW(menu, MF_STRING | MF_GRAYED, id, w!("Performance (needs charger)"))?;
            } else {
                AppendMenuW(menu, MF_STRING, id, &HSTRING::from(mode.name()))?;
            }
        }
        if let Ok(s) = state
            && let Some(i) = s.mode().and_then(|m| Mode::ALL.iter().position(|&x| x == m))
        {
            let last = ID_MODE_FIRST + Mode::ALL.len() as u32 - 1;
            CheckMenuRadioItem(menu, ID_MODE_FIRST, last, ID_MODE_FIRST + i as u32, MF_BYCOMMAND.0)?;
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        let max_on = matches!(state, Ok(s) if s.max_fan == MaxFan::Max);
        let check = if max_on { MF_CHECKED } else { MF_UNCHECKED };
        AppendMenuW(menu, MF_STRING | check, ID_MAX_FAN as usize, w!("Max fan"))?;
        AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        if autostart || autostart::running_installed() {
            let check = if autostart { MF_CHECKED } else { MF_UNCHECKED };
            AppendMenuW(menu, MF_STRING | check, ID_AUTOSTART as usize, w!("Start with Windows"))?;
        } else {
            let label = w!("Start with Windows (run --install)");
            AppendMenuW(menu, MF_STRING | MF_GRAYED, ID_AUTOSTART as usize, label)?;
        }
        AppendMenuW(menu, MF_STRING, ID_EXIT as usize, w!("Exit"))?;
        Ok(menu)
    }
}

fn mode_name(s: &State) -> &'static str {
    s.mode().map_or("Unknown", Mode::name)
}

fn status_line(state: &Result<State, victus::Error>) -> String {
    match state {
        Ok(s) => format!("Mode: {} · Fans {} / {} rpm", mode_name(s), s.fan_rpm[0], s.fan_rpm[1]),
        Err(e) => format!("Can't read BIOS: {e}"),
    }
}

fn tooltip(state: &Result<State, victus::Error>) -> String {
    match state {
        Ok(s) => format!("Victus: {} · fan {}", mode_name(s), s.max_fan),
        Err(_) => "Victus: can't read BIOS".to_string(),
    }
}

fn fan_name(on: bool) -> &'static str {
    if on { "max fan" } else { "fan AUTO" }
}

fn on_ac_power() -> Option<bool> {
    let mut status = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut status) }.ok()?;
    match status.ACLineStatus {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// On battery the BIOS forces Default and silently ignores a Performance SET.
fn on_battery() -> bool {
    on_ac_power() == Some(false)
}

fn power_name(on_ac: Option<bool>) -> &'static str {
    match on_ac {
        Some(true) => "AC",
        Some(false) => "battery",
        None => "unknown power",
    }
}

/// Copies `s` into a fixed, NUL-terminated UTF-16 buffer, truncating if needed.
fn copy_wide(dst: &mut [u16], s: &str) {
    let max = dst.len() - 1;
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(max)) {
        *d = c;
        n += 1;
    }
    dst[n] = 0;
}

impl App {
    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }

    fn add_icon(&self) {
        let mut d = self.data();
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = self.icon;
        copy_wide(&mut d.szTip, &self.tip);
        d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &d);
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
        }
    }

    fn remove_icon(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
        }
    }

    fn set_tip(&mut self, tip: String) {
        if tip == self.tip {
            return;
        }
        self.tip = tip;
        let mut d = self.data();
        d.uFlags = NIF_TIP | NIF_SHOWTIP;
        copy_wide(&mut d.szTip, &self.tip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }

    fn refresh_tip(&mut self) {
        let state = self.victus.read_state();
        self.set_tip(tooltip(&state));
    }

    fn notify(&self, flags: NOTIFY_ICON_INFOTIP_FLAGS, title: &str, text: &str) {
        let mut d = self.data();
        d.uFlags = NIF_INFO;
        d.dwInfoFlags = flags;
        copy_wide(&mut d.szInfoTitle, title);
        copy_wide(&mut d.szInfo, text);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }

    fn remember(&self, update: impl FnOnce(&mut Config)) {
        let mut cfg = Config::load();
        update(&mut cfg);
        if let Err(e) = cfg.save() {
            log::write(&format!("can't save config: {e}"));
            self.notify(NIIF_WARNING, "Couldn't save the setting", &e.to_string());
        }
    }

    fn choose_mode(&mut self, mode: Mode) {
        log::write(&format!("menu: {mode}"));
        if self.apply_mode(mode) {
            self.remember(|cfg| cfg.mode = Some(mode));
        }
    }

    fn choose_max_fan(&mut self, on: bool) {
        log::write(&format!("menu: {}", fan_name(on)));
        if self.apply_max_fan(on) {
            self.remember(|cfg| cfg.max_fan = Some(on));
        }
        self.refresh_tip();
    }

    /// Starts the read-back check on success; notifies on failure.
    fn apply_mode(&mut self, mode: Mode) -> bool {
        if let Err(e) = self.victus.set_mode(mode) {
            log::write(&format!("set {mode} failed: {e}"));
            self.notify(NIIF_ERROR, &format!("Couldn't set {mode}"), &e.to_string());
            self.refresh_tip();
            return false;
        }
        self.verifying = Some((mode, Instant::now()));
        unsafe { SetTimer(Some(self.hwnd), VERIFY_TIMER, VERIFY_POLL_MS, None) };
        true
    }

    fn apply_max_fan(&mut self, on: bool) -> bool {
        if let Err(e) = self.victus.set_max_fan(on) {
            log::write(&format!("set {} failed: {e}", fan_name(on)));
            self.notify(NIIF_ERROR, &format!("Couldn't set {}", fan_name(on)), &e.to_string());
            return false;
        }
        true
    }

    /// Polled after a mode SET until the BIOS mode indicator shows the new mode.
    fn check_mode_applied(&mut self) {
        if let Some((mode, started)) = self.verifying {
            let shown = self.victus.read_mode();
            if !matches!(shown, Ok(Some(m)) if m == mode) {
                if started.elapsed() < VERIFY_TIMEOUT {
                    return;
                }
                let now = match shown {
                    Ok(Some(m)) => m.name().to_string(),
                    Ok(None) => "an unknown mode".to_string(),
                    Err(e) => format!("an error ({e})"),
                };
                log::write(&format!("{mode} not confirmed after 3 s: BIOS reports {now}"));
                self.notify(NIIF_WARNING, &format!("{mode} not confirmed"), &format!("The BIOS still reports {now}."));
            }
        }
        self.verifying = None;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), VERIFY_TIMER);
        }
        self.refresh_tip();
    }

    fn power_source_changed(&mut self, on_ac: bool) {
        let was = self.on_ac.replace(on_ac);
        if was == Some(on_ac) {
            return;
        }
        log::write(&format!("power source: {}", power_name(Some(on_ac))));
        if on_ac {
            self.schedule_reapply("AC plug-in");
        }
    }

    fn toggle_autostart(&mut self) {
        let result = if self.autostart { autostart::disable() } else { autostart::enable() };
        match result {
            Ok(()) => {
                self.autostart = !self.autostart;
                log::write(if self.autostart { "menu: start with Windows on" } else { "menu: start with Windows off" });
            }
            Err(e) => {
                log::write(&format!("start with Windows: {e}"));
                self.notify(NIIF_ERROR, "Couldn't change Start with Windows", &e);
            }
        }
    }

    /// The first notification only reports the current state.
    fn display_changed(&mut self, on: bool) {
        let was = self.display_on.replace(on);
        if was.is_none() || was == Some(on) {
            return;
        }
        log::write(if on { "display on" } else { "display off" });
        if on {
            self.schedule_reapply("display on");
        }
    }

    fn schedule_reapply(&mut self, reason: &'static str) {
        self.reapply_reason = Some(reason);
        unsafe { SetTimer(Some(self.hwnd), REAPPLY_TIMER, REAPPLY_DELAY_MS, None) };
    }

    fn run_scheduled_reapply(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), REAPPLY_TIMER);
        }
        if let Some(reason) = self.reapply_reason.take() {
            self.reapply(reason);
        }
    }

    /// Sets whatever differs between the BIOS and the remembered setting.
    fn reapply(&mut self, reason: &str) {
        let cfg = Config::load();
        let power = power_name(on_ac_power());
        let state = match self.victus.read_state() {
            Ok(s) => s,
            Err(e) => {
                log::write(&format!("{reason} ({power}): can't read BIOS: {e}"));
                self.notify(NIIF_ERROR, "Couldn't re-apply the setting", &e.to_string());
                return;
            }
        };
        let mut changes = Vec::new();
        let mut note = "";
        if let Some(mode) = cfg.mode
            && state.mode() != Some(mode)
        {
            if mode == Mode::Performance && on_battery() {
                note = " (Performance waits for AC)";
            } else {
                changes.push(mode.name());
                self.apply_mode(mode);
            }
        }
        if let Some(on) = cfg.max_fan
            && on != (state.max_fan == MaxFan::Max)
        {
            changes.push(fan_name(on));
            self.apply_max_fan(on);
        }
        let remembered = match (cfg.mode, cfg.max_fan) {
            (None, None) => "nothing".to_string(),
            (m, f) => format!(
                "{} / {}",
                m.map_or("-", Mode::name),
                f.map_or("-", fan_name)
            ),
        };
        let action = if changes.is_empty() { "no change".to_string() } else { format!("set {}", changes.join(", ")) };
        log::write(&format!(
            "{reason} ({power}): BIOS {} (indicator {}) / fan {}, remembered {remembered}: {action}{note}",
            mode_name(&state),
            state.indicator,
            state.max_fan
        ));
        self.refresh_tip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_wide_terminates_and_truncates() {
        let mut buf = [0xFFFFu16; 4];
        copy_wide(&mut buf, "ab");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, 0, 0xFFFF]);
        copy_wide(&mut buf, "abcdef");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }
}
