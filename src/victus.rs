//! HP Victus (board 8A25) requests and mode recipes.
//!
//! Only the requests in `Get` / `Set` can be sent, and SET requests are
//! refused on other boards unless forced.

use std::fmt;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::w;

use crate::bios::{self, Bios, Method};

/// The only board the protocol was verified on.
pub const SUPPORTED_BOARD: &str = "8A25";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Quiet,
    Default,
    Performance,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Quiet, Mode::Default, Mode::Performance];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Quiet => "Quiet",
            Mode::Default => "Default",
            Mode::Performance => "Performance",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name().eq_ignore_ascii_case(s))
    }

    /// Second byte of the `0x1A` payload.
    fn bios_value(self) -> u8 {
        match self {
            Mode::Default => 0x00,
            Mode::Performance => 0x01,
            Mode::Quiet => 0x03,
        }
    }

    /// Byte 0 of the `0x11` reply while this mode is active.
    fn indicator(self) -> u8 {
        match self {
            Mode::Quiet => 0x18,
            Mode::Default => 0x28,
            Mode::Performance => 0x2D,
        }
    }

    pub fn from_indicator(b: u8) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.indicator() == b)
    }

    /// b0 = GPU boost (on only in Performance), b1 = Dynamic Boost on,
    /// b2/b3 kept from the BIOS.
    fn gpu_bytes(self, current: [u8; 4]) -> [u8; 4] {
        [(self == Mode::Performance) as u8, 0x01, current[2], current[3]]
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// GET requests (all sent with `hpqBIOSInt128`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Get {
    /// Fan 0 or 1. The reply also carries the mode indicator.
    FanSpeed(u8),
    MaxFan,
    GpuPower,
}

impl Get {
    fn request(self) -> (u32, Vec<u8>) {
        match self {
            Get::FanSpeed(fan) => (0x11, vec![fan]),
            Get::MaxFan => (0x26, vec![0, 0, 0, 0]),
            Get::GpuPower => (0x21, vec![0, 0, 0, 0]),
        }
    }
}

/// SET requests (all sent with `hpqBIOSInt0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Set {
    Mode(Mode),
    MaxFan(bool),
    GpuPower([u8; 4]),
}

impl Set {
    fn request(self) -> (u32, Vec<u8>) {
        match self {
            Set::Mode(m) => (0x1A, vec![0xFF, m.bios_value()]),
            Set::MaxFan(on) => (0x27, vec![on as u8, 0, 0, 0]),
            Set::GpuPower(b) => (0x22, b.to_vec()),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Bios(bios::Error),
    WrongBoard(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bios(e) => e.fmt(f),
            Error::WrongBoard(b) => write!(
                f,
                "board is '{b}', not {SUPPORTED_BOARD}: refusing to send SET requests (use --force to override)"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<bios::Error> for Error {
    fn from(e: bios::Error) -> Self {
        Error::Bios(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FanReading {
    indicator: u8,
    rpm: u16,
}

fn parse_fan(data: &[u8]) -> std::result::Result<FanReading, bios::Error> {
    match data {
        [indicator, _, hi, lo, ..] => Ok(FanReading { indicator: *indicator, rpm: u16::from_be_bytes([*hi, *lo]) }),
        _ => Err(bios::Error::BadReply("fan reply shorter than 4 bytes")),
    }
}

fn first4(data: &[u8]) -> std::result::Result<[u8; 4], bios::Error> {
    data.get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or(bios::Error::BadReply("reply shorter than 4 bytes"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaxFan {
    Auto,
    Max,
    Unknown(u8),
}

impl MaxFan {
    fn from_byte(b: u8) -> MaxFan {
        match b {
            0 => MaxFan::Auto,
            1 => MaxFan::Max,
            other => MaxFan::Unknown(other),
        }
    }
}

impl fmt::Display for MaxFan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MaxFan::Auto => f.write_str("AUTO"),
            MaxFan::Max => f.write_str("MAX"),
            MaxFan::Unknown(b) => write!(f, "unknown ({b})"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct State {
    pub indicator: u8,
    pub fan_rpm: [u16; 2],
    pub max_fan: MaxFan,
    pub gpu: [u8; 4],
}

impl State {
    pub fn mode(&self) -> Option<Mode> {
        Mode::from_indicator(self.indicator)
    }
}

pub struct Victus {
    bios: Bios,
    board: String,
    allow_set: bool,
}

impl Victus {
    /// With `force`, SET requests are allowed on boards other than 8A25.
    pub fn open(force: bool) -> Result<Victus> {
        let board = board_id();
        let allow_set = force || board == SUPPORTED_BOARD;
        Ok(Victus { bios: Bios::connect()?, board, allow_set })
    }

    pub fn board(&self) -> &str {
        &self.board
    }

    fn get(&self, req: Get) -> Result<Vec<u8>> {
        let (ty, payload) = req.request();
        Ok(self.bios.call(Method::Int128, ty, &payload)?)
    }

    fn set(&self, req: Set) -> Result<()> {
        if !self.allow_set {
            return Err(Error::WrongBoard(self.board.clone()));
        }
        let (ty, payload) = req.request();
        self.bios.call(Method::Int0, ty, &payload)?;
        Ok(())
    }

    fn fan(&self, fan: u8) -> Result<FanReading> {
        Ok(parse_fan(&self.get(Get::FanSpeed(fan))?)?)
    }

    fn gpu(&self) -> Result<[u8; 4]> {
        Ok(first4(&self.get(Get::GpuPower)?)?)
    }

    pub fn read_mode(&self) -> Result<Option<Mode>> {
        Ok(Mode::from_indicator(self.fan(0)?.indicator))
    }

    pub fn read_state(&self) -> Result<State> {
        let f0 = self.fan(0)?;
        let f1 = self.fan(1)?;
        let max = self.get(Get::MaxFan)?;
        let max_fan = MaxFan::from_byte(*max.first().ok_or(bios::Error::BadReply("empty max fan reply"))?);
        Ok(State { indicator: f0.indicator, fan_rpm: [f0.rpm, f1.rpm], max_fan, gpu: self.gpu()? })
    }

    /// Sets a mode the way Gaming Hub does: mode first, then the GPU bytes
    /// (the mode SET changes them by itself, so the GPU SET must come after).
    pub fn set_mode(&self, mode: Mode) -> Result<()> {
        self.set(Set::Mode(mode))?;
        let gpu = mode.gpu_bytes(self.gpu()?);
        self.set(Set::GpuPower(gpu))
    }

    pub fn wait_for_mode(&self, mode: Mode, timeout: Duration) -> Result<bool> {
        let start = Instant::now();
        loop {
            if self.read_mode()? == Some(mode) {
                return Ok(true);
            }
            if start.elapsed() >= timeout {
                return Ok(false);
            }
            sleep(Duration::from_millis(250));
        }
    }

    pub fn set_max_fan(&self, on: bool) -> Result<()> {
        self.set(Set::MaxFan(on))
    }
}

/// SMBIOS baseboard product (same value as `Win32_BaseBoard.Product`),
/// or an empty string if it can't be read.
fn board_id() -> String {
    let mut buf = [0u16; 64];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("HARDWARE\\DESCRIPTION\\System\\BIOS"),
            w!("BaseBoardProduct"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_request_payloads() {
        assert_eq!(Set::Mode(Mode::Quiet).request(), (0x1A, vec![0xFF, 0x03]));
        assert_eq!(Set::Mode(Mode::Default).request(), (0x1A, vec![0xFF, 0x00]));
        assert_eq!(Set::Mode(Mode::Performance).request(), (0x1A, vec![0xFF, 0x01]));
    }

    #[test]
    fn max_fan_request_payloads() {
        assert_eq!(Set::MaxFan(true).request(), (0x27, vec![1, 0, 0, 0]));
        assert_eq!(Set::MaxFan(false).request(), (0x27, vec![0, 0, 0, 0]));
        assert_eq!(Get::MaxFan.request(), (0x26, vec![0, 0, 0, 0]));
    }

    #[test]
    fn get_request_payloads() {
        assert_eq!(Get::FanSpeed(0).request(), (0x11, vec![0]));
        assert_eq!(Get::FanSpeed(1).request(), (0x11, vec![1]));
        assert_eq!(Get::GpuPower.request(), (0x21, vec![0, 0, 0, 0]));
    }

    #[test]
    fn gpu_bytes_per_mode() {
        // Starting from what the BIOS leaves after a Quiet SET.
        let bios = [0x00, 0x00, 0x01, 0x00];
        assert_eq!(Mode::Quiet.gpu_bytes(bios), [0x00, 0x01, 0x01, 0x00]);
        assert_eq!(Mode::Default.gpu_bytes(bios), [0x00, 0x01, 0x01, 0x00]);
        assert_eq!(Mode::Performance.gpu_bytes(bios), [0x01, 0x01, 0x01, 0x00]);
        assert_eq!(Mode::Performance.gpu_bytes([0, 0, 0x07, 0x09]), [0x01, 0x01, 0x07, 0x09]);
    }

    #[test]
    fn indicator_maps_to_mode() {
        assert_eq!(Mode::from_indicator(24), Some(Mode::Quiet));
        assert_eq!(Mode::from_indicator(40), Some(Mode::Default));
        assert_eq!(Mode::from_indicator(45), Some(Mode::Performance));
        assert_eq!(Mode::from_indicator(0), None);
        assert_eq!(Mode::from_indicator(0x3C), None);
    }

    #[test]
    fn fan_reply_parses() {
        // A real reply: indicator 45, constant 0x3C, 2013 rpm.
        let r = parse_fan(&[45, 0x3C, 0x07, 0xDD, 0, 0]).unwrap();
        assert_eq!(r, FanReading { indicator: 45, rpm: 2013 });
        assert!(parse_fan(&[45, 0x3C, 0x07]).is_err());
    }

    #[test]
    fn mode_names_parse() {
        assert_eq!(Mode::parse("quiet"), Some(Mode::Quiet));
        assert_eq!(Mode::parse("PERFORMANCE"), Some(Mode::Performance));
        assert_eq!(Mode::parse("eco"), None);
    }
}
