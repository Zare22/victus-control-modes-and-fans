//! victus-control — performance mode and max fan for the HP Victus 16-d1xxx (board 8A25).

mod bios;
mod config;
mod icon;
mod log;
mod tray;
mod victus;

use std::process::ExitCode;
use std::time::Duration;

use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
use windows::core::{HSTRING, w};

use victus::{Mode, State, Victus};

const USAGE: &str = "\
usage: victus-control [<command>] [--force]

  (no command)                       run the tray icon
  status                             show mode, max fan, fan speeds, GPU power
  mode quiet|default|performance     set the performance mode
  fan max|auto                       set max fan on / back to AUTO

  --force   allow SET requests on boards other than 8A25 (unverified!)";

enum Command {
    Status,
    Mode(Mode),
    Fan(bool),
}

/// A `None` command means: run the tray icon.
fn parse_args(args: &[String]) -> Option<(Option<Command>, bool)> {
    let force = args.iter().any(|a| a == "--force");
    let rest: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--force").collect();
    let cmd = match rest.as_slice() {
        [] => None,
        ["status"] => Some(Command::Status),
        ["mode", m] => Some(Command::Mode(Mode::parse(m)?)),
        ["fan", f] if f.eq_ignore_ascii_case("max") => Some(Command::Fan(true)),
        ["fan", f] if f.eq_ignore_ascii_case("auto") => Some(Command::Fan(false)),
        _ => return None,
    };
    Some((cmd, force))
}

fn print_state(v: &Victus, s: &State) {
    let mode = match s.mode() {
        Some(m) => m.name(),
        None => "Unknown",
    };
    let g = s.gpu;
    println!("Board      {}", v.board());
    println!("Mode       {mode} (indicator {})", s.indicator);
    println!("Max fan    {}", s.max_fan);
    println!("Fans       {} / {} rpm", s.fan_rpm[0], s.fan_rpm[1]);
    println!(
        "GPU power  {:02X} {:02X} {:02X} {:02X} (GPU boost {}, Dynamic Boost {})",
        g[0],
        g[1],
        g[2],
        g[3],
        on_off(g[0]),
        on_off(g[1])
    );
}

fn on_off(b: u8) -> &'static str {
    if b != 0 { "on" } else { "off" }
}

fn run(cmd: Command, force: bool) -> Result<ExitCode, victus::Error> {
    let v = Victus::open(force)?;
    match cmd {
        Command::Status => {
            print_state(&v, &v.read_state()?);
        }
        Command::Mode(mode) => {
            v.set_mode(mode)?;
            log::write(&format!("cli: {mode}"));
            remember(|cfg| cfg.mode = Some(mode));
            let ok = v.wait_for_mode(mode, Duration::from_secs(3))?;
            print_state(&v, &v.read_state()?);
            if !ok {
                eprintln!("warning: mode indicator doesn't show {mode} after 3 s");
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Fan(on) => {
            v.set_max_fan(on)?;
            log::write(&format!("cli: {}", if on { "max fan" } else { "fan AUTO" }));
            remember(|cfg| cfg.max_fan = Some(on));
            print_state(&v, &v.read_state()?);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn remember(update: impl FnOnce(&mut config::Config)) {
    let mut cfg = config::Config::load();
    update(&mut cfg);
    if let Err(e) = cfg.save() {
        eprintln!("warning: couldn't save the setting: {e}");
    }
}

fn run_tray(force: bool) -> ExitCode {
    match tray::run(force) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Usually started without a console, so stderr may go nowhere.
            unsafe { MessageBoxW(None, &HSTRING::from(e), w!("victus-control"), MB_OK | MB_ICONERROR) };
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, force)) = parse_args(&args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let Some(cmd) = cmd else {
        return run_tray(force);
    };
    match run(cmd, force) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
