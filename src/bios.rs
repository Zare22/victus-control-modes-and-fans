//! Raw transport to the HP BIOS WMI interface (`root\wmi` : `hpqBIntM`).
//!
//! Sends any request it is given; `victus` decides which requests are allowed.

use std::fmt;
use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RPC_E_TOO_LATE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoInitializeSecurity, CoSetProxyBlanket, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IMPERSONATE, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayCreateVector, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayUnaccessData,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_BSTR, VT_I4, VT_UI1,
    VT_UNKNOWN, VariantClear,
};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_FLAG_RETURN_WBEM_COMPLETE, WBEM_GENERIC_FLAG_TYPE,
    WBEM_INFINITE, WbemLocator,
};
use windows::core::{BSTR, IUnknown, Interface, PCWSTR, w};

const SIGN: [u8; 4] = *b"SECU";
const COMMAND: u32 = 0x20008;
/// Returned for `hpqBIntM` to any non-elevated process.
const WBEM_E_ACCESS_DENIED: u32 = 0x8004_1003;

/// The number in `hpqBIOSInt*` is the size of the reply buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// No reply data — used for SET requests.
    Int0,
    /// 128-byte reply — used for GET requests.
    Int128,
}

impl Method {
    fn name(self) -> PCWSTR {
        match self {
            Method::Int0 => w!("hpqBIOSInt0"),
            Method::Int128 => w!("hpqBIOSInt128"),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Com { what: &'static str, err: windows::core::Error },
    /// Non-zero `rwReturnCode`.
    Bios(u32),
    BadReply(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn return_code_text(code: u32) -> &'static str {
    match code {
        0 => "OK",
        1 => "error",
        2 => "wrong signature",
        3 => "unknown command",
        4 => "command type not supported on this model",
        5 => "invalid parameters",
        _ => "unknown return code",
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Com { what, err } => {
                let code = err.code().0 as u32;
                write!(f, "{what} failed: {} (0x{code:08X})", err.message().trim())?;
                if code == WBEM_E_ACCESS_DENIED {
                    f.write_str(" — the HP BIOS interface needs administrator rights")?;
                }
                Ok(())
            }
            Error::Bios(code) => write!(f, "BIOS returned {code} ({})", return_code_text(*code)),
            Error::BadReply(what) => write!(f, "unexpected BIOS reply: {what}"),
        }
    }
}

impl std::error::Error for Error {}

trait Context<T> {
    fn ctx(self, what: &'static str) -> Result<T>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, what: &'static str) -> Result<T> {
        self.map_err(|err| Error::Com { what, err })
    }
}

pub struct Bios {
    svc: IWbemServices,
    /// e.g. `hpqBIntM.InstanceName="ACPI\\PNP0C14\\0_0"`
    path: BSTR,
    data_in_class: IWbemClassObject,
    int0_params: IWbemClassObject,
    int128_params: IWbemClassObject,
}

impl Bios {
    /// Initializes COM on the calling thread as MTA, so BIOS calls block
    /// instead of dispatching window messages while they wait.
    pub fn connect() -> Result<Self> {
        unsafe {
            // S_FALSE (already initialized) is fine; a different apartment model is not.
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().ctx("CoInitializeEx")?;
            if let Err(e) = CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_DEFAULT,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            ) && e.code() != RPC_E_TOO_LATE
            {
                return Err(Error::Com { what: "CoInitializeSecurity", err: e });
            }

            let locator: IWbemLocator =
                CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ctx("create WbemLocator")?;
            let svc = locator
                .ConnectServer(
                    &BSTR::from("ROOT\\WMI"),
                    &BSTR::new(),
                    &BSTR::new(),
                    &BSTR::new(),
                    0,
                    &BSTR::new(),
                    None,
                )
                .ctx("connect to root\\wmi")?;
            CoSetProxyBlanket(
                &svc,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
            .ctx("CoSetProxyBlanket")?;

            let flags = WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_RETURN_IMMEDIATELY.0 | WBEM_FLAG_FORWARD_ONLY.0);
            let instances = svc
                .CreateInstanceEnum(&BSTR::from("hpqBIntM"), flags, None)
                .ctx("enumerate hpqBIntM (no HP BIOS WMI interface?)")?;
            let mut found = [None];
            let mut count = 0u32;
            instances.Next(WBEM_INFINITE, &mut found, &mut count).ok().ctx("read hpqBIntM instance")?;
            let instance = found[0].take().ok_or(Error::BadReply("no hpqBIntM instance"))?;
            let path = get(&instance, w!("__RELPATH"))?.to_bstr()?;

            let class = get_object(&svc, "hpqBIntM")?;
            let data_in_class = get_object(&svc, "hpqBDataIn")?;
            let int0_params = method_params(&class, Method::Int0)?;
            let int128_params = method_params(&class, Method::Int128)?;

            Ok(Bios { svc, path, data_in_class, int0_params, int128_params })
        }
    }

    /// Returns the reply `Data` (empty for `Int0`).
    /// A non-zero `rwReturnCode` becomes `Error::Bios`.
    pub fn call(&self, method: Method, command_type: u32, payload: &[u8]) -> Result<Vec<u8>> {
        unsafe {
            let data_in = self.data_in_class.SpawnInstance(0).ctx("spawn hpqBDataIn")?;
            put(&data_in, w!("Sign"), Var::bytes(&SIGN)?)?;
            put(&data_in, w!("Command"), Var::i4(COMMAND as i32))?;
            put(&data_in, w!("CommandType"), Var::i4(command_type as i32))?;
            put(&data_in, w!("Size"), Var::i4(payload.len() as i32))?;
            put(&data_in, w!("hpqBData"), Var::bytes(payload)?)?;

            let param_class = match method {
                Method::Int0 => &self.int0_params,
                Method::Int128 => &self.int128_params,
            };
            let params = param_class.SpawnInstance(0).ctx("spawn method parameters")?;
            let unk: IUnknown = data_in.cast().ctx("cast hpqBDataIn")?;
            put(&params, w!("InData"), Var::unknown(unk))?;

            let mut out = None;
            self.svc
                .ExecMethod(
                    &self.path,
                    &BSTR::from_wide(method.name().as_wide()),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    &params,
                    Some(&mut out),
                    None,
                )
                .ctx("ExecMethod")?;
            let out = out.ok_or(Error::BadReply("no output parameters"))?;
            let out_data: IWbemClassObject =
                get(&out, w!("OutData"))?.to_unknown()?.cast().ctx("cast OutData")?;

            let code = get(&out_data, w!("rwReturnCode"))?.to_i4()? as u32;
            if code != 0 {
                return Err(Error::Bios(code));
            }
            match method {
                Method::Int0 => Ok(Vec::new()),
                Method::Int128 => get(&out_data, w!("Data"))?.to_bytes(),
            }
        }
    }
}

unsafe fn get_object(svc: &IWbemServices, name: &'static str) -> Result<IWbemClassObject> {
    let mut obj = None;
    unsafe {
        svc.GetObject(&BSTR::from(name), WBEM_FLAG_RETURN_WBEM_COMPLETE, None, Some(&mut obj), None)
            .ctx("get WMI class")?;
    }
    obj.ok_or(Error::BadReply("WMI class missing"))
}

unsafe fn method_params(class: &IWbemClassObject, method: Method) -> Result<IWbemClassObject> {
    let (mut input, mut output) = (None, None);
    unsafe { class.GetMethod(method.name(), 0, &mut input, &mut output).ctx("get method signature")? };
    input.ok_or(Error::BadReply("method has no input parameters"))
}

unsafe fn get(obj: &IWbemClassObject, name: PCWSTR) -> Result<Var> {
    let mut v = Var(VARIANT::default());
    unsafe { obj.Get(name, 0, &mut v.0, None, None).ctx("read WMI property")? };
    Ok(v)
}

unsafe fn put(obj: &IWbemClassObject, name: PCWSTR, value: Var) -> Result<()> {
    unsafe { obj.Put(name, 0, &value.0, 0).ctx("set WMI property") }
}

/// A VARIANT that is cleared (and its contents released) on drop.
struct Var(VARIANT);

impl Drop for Var {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

impl Var {
    fn with(vt: VARENUM, value: VARIANT_0_0_0) -> Var {
        Var(VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                    vt,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: value,
                }),
            },
        })
    }

    /// WMI passes CIM uint32 as VT_I4.
    fn i4(v: i32) -> Var {
        Var::with(VT_I4, VARIANT_0_0_0 { lVal: v })
    }

    fn unknown(v: IUnknown) -> Var {
        Var::with(VT_UNKNOWN, VARIANT_0_0_0 { punkVal: ManuallyDrop::new(Some(v)) })
    }

    fn bytes(data: &[u8]) -> Result<Var> {
        unsafe {
            let psa = SafeArrayCreateVector(VT_UI1, 0, data.len() as u32);
            if psa.is_null() {
                return Err(Error::BadReply("SafeArrayCreateVector failed"));
            }
            // Owned by the VARIANT from here on (freed by VariantClear).
            let var = Var::with(VARENUM(VT_ARRAY.0 | VT_UI1.0), VARIANT_0_0_0 { parray: psa });
            if !data.is_empty() {
                let mut p = std::ptr::null_mut();
                SafeArrayAccessData(psa, &mut p).ctx("SafeArrayAccessData")?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u8>(), data.len());
                SafeArrayUnaccessData(psa).ctx("SafeArrayUnaccessData")?;
            }
            Ok(var)
        }
    }

    fn vt(&self) -> VARENUM {
        unsafe { self.0.Anonymous.Anonymous.vt }
    }

    fn to_i4(&self) -> Result<i32> {
        match self.vt() {
            VT_I4 => Ok(unsafe { self.0.Anonymous.Anonymous.Anonymous.lVal }),
            _ => Err(Error::BadReply("expected an integer")),
        }
    }

    fn to_bstr(&self) -> Result<BSTR> {
        match self.vt() {
            VT_BSTR => Ok(unsafe { (*self.0.Anonymous.Anonymous.Anonymous.bstrVal).clone() }),
            _ => Err(Error::BadReply("expected a string")),
        }
    }

    fn to_unknown(&self) -> Result<IUnknown> {
        match self.vt() {
            VT_UNKNOWN => unsafe { (*self.0.Anonymous.Anonymous.Anonymous.punkVal).clone() }
                .ok_or(Error::BadReply("null object")),
            _ => Err(Error::BadReply("expected an object")),
        }
    }

    fn to_bytes(&self) -> Result<Vec<u8>> {
        if self.vt() != VARENUM(VT_ARRAY.0 | VT_UI1.0) {
            return Err(Error::BadReply("expected a byte array"));
        }
        unsafe {
            let psa: *mut SAFEARRAY = self.0.Anonymous.Anonymous.Anonymous.parray;
            let lo = SafeArrayGetLBound(psa, 1).ctx("SafeArrayGetLBound")?;
            let hi = SafeArrayGetUBound(psa, 1).ctx("SafeArrayGetUBound")?;
            let len = (hi - lo + 1).max(0) as usize;
            let mut out = vec![0u8; len];
            if len > 0 {
                let mut p = std::ptr::null_mut();
                SafeArrayAccessData(psa, &mut p).ctx("SafeArrayAccessData")?;
                std::ptr::copy_nonoverlapping(p.cast::<u8>(), out.as_mut_ptr(), len);
                SafeArrayUnaccessData(psa).ctx("SafeArrayUnaccessData")?;
            }
            Ok(out)
        }
    }
}
