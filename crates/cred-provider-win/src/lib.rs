// Phantom Credential Provider — minimal auto-submit CP reading creds from
// C:\ProgramData\phantom\auth and packing them as KERB_INTERACTIVE_UNLOCK_LOGON
// for LSA. MVP: plaintext password in file (Phase 1 → S4U after wiring works).

#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use windows::core::{implement, Interface, Result, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    BOOL, CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_NOINTERFACE,
    E_NOTIMPL, E_POINTER, HANDLE, NTSTATUS, S_FALSE, S_OK,
};
use windows::Win32::Graphics::Gdi::HBITMAP;
// MSV1_0_S4U_LOGON is not exposed in windows 0.58 — define it manually.
// https://learn.microsoft.com/en-us/windows/win32/api/ntsecapi/ns-ntsecapi-msv1_0_s4u_logon
use windows::Win32::Foundation::UNICODE_STRING;
use windows::Win32::System::Com::{CoTaskMemAlloc, IClassFactory, IClassFactory_Impl};
use windows::Win32::UI::Shell::*;

// ccd145e9-71bb-4e91-a604-2ee449adfd54
pub const PHANTOM_CP_CLSID: GUID = GUID::from_u128(0xccd145e9_71bb_4e91_a604_2ee449adfd54);

const AUTH_FILE: &str = r"C:\ProgramData\phantom\auth";
/// How long a ticket is considered fresh after phantom-server wrote it.
/// Beyond this window, the ticket is treated as absent (prevents stale
/// tickets from auto-logging in the wrong user later).
const TICKET_MAX_AGE_SECS: u64 = 10;

static G_REF: AtomicI32 = AtomicI32::new(0);
fn dll_addref() {
    G_REF.fetch_add(1, Ordering::SeqCst);
}
fn dll_release() {
    G_REF.fetch_sub(1, Ordering::SeqCst);
}

// Usage scenario is set once by LogonUI per CP instance; Tile reads it back
// at GetSerialization time. Global is simpler than threading through via
// the #[implement] macro's wrapper type.
static CPUS_STATE: AtomicU32 = AtomicU32::new(0);

// -----------------------------------------------------------------------------
// DLL exports
// -----------------------------------------------------------------------------
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = ptr::null_mut();
    if rclsid.is_null() || riid.is_null() {
        return E_INVALIDARG;
    }
    if *rclsid != PHANTOM_CP_CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    if *riid != IClassFactory::IID {
        return E_NOINTERFACE;
    }
    let factory: IClassFactory = Factory.into();
    *ppv = mem::transmute(factory);
    S_OK
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if G_REF.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

// -----------------------------------------------------------------------------
// IClassFactory
// -----------------------------------------------------------------------------
#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Option<&windows::core::IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if ppv.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *ppv = ptr::null_mut();
            if *riid != ICredentialProvider::IID {
                return Err(E_NOINTERFACE.into());
            }
            let p: ICredentialProvider = Provider.into();
            *ppv = mem::transmute(p);
        }
        Ok(())
    }

    fn LockServer(&self, lock: BOOL) -> Result<()> {
        if lock.as_bool() {
            dll_addref();
        } else {
            dll_release();
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ICredentialProvider
// -----------------------------------------------------------------------------
#[implement(ICredentialProvider)]
struct Provider;

impl ICredentialProvider_Impl for Provider_Impl {
    fn SetUsageScenario(
        &self,
        cpus: CREDENTIAL_PROVIDER_USAGE_SCENARIO,
        _flags: u32,
    ) -> Result<()> {
        match cpus {
            CPUS_LOGON | CPUS_UNLOCK_WORKSTATION | CPUS_CREDUI => {
                CPUS_STATE.store(cpus.0 as u32, Ordering::SeqCst);
                Ok(())
            }
            _ => Err(E_NOTIMPL.into()),
        }
    }

    fn SetSerialization(
        &self,
        _pcpcs: *const CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Advise(
        &self,
        _events: Option<&ICredentialProviderEvents>,
        _context: usize,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnAdvise(&self) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetFieldDescriptorCount(&self) -> Result<u32> {
        Ok(1)
    }

    fn GetFieldDescriptorAt(
        &self,
        idx: u32,
    ) -> Result<*mut CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR> {
        if idx != 0 {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            let p = CoTaskMemAlloc(mem::size_of::<CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR>())
                as *mut CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR;
            if p.is_null() {
                return Err(E_POINTER.into());
            }
            (*p).dwFieldID = 0;
            (*p).cpft = CPFT_LARGE_TEXT;
            (*p).pszLabel = alloc_wide("Phantom");
            (*p).guidFieldType = CPFG_CREDENTIAL_PROVIDER_LABEL;
            Ok(p)
        }
    }

    fn GetCredentialCount(
        &self,
        pdwCount: *mut u32,
        pdwDefault: *mut u32,
        pbAutoLogonWithDefault: *mut BOOL,
    ) -> Result<()> {
        unsafe {
            *pdwCount = 1;
            *pdwDefault = 0;
            *pbAutoLogonWithDefault = BOOL(1);
        }
        Ok(())
    }

    fn GetCredentialAt(&self, idx: u32) -> Result<ICredentialProviderCredential> {
        if idx != 0 {
            return Err(E_INVALIDARG.into());
        }
        Ok(Tile.into())
    }
}

// -----------------------------------------------------------------------------
// ICredentialProviderCredential
// -----------------------------------------------------------------------------
#[implement(ICredentialProviderCredential)]
struct Tile;

impl ICredentialProviderCredential_Impl for Tile_Impl {
    fn Advise(
        &self,
        _e: Option<&ICredentialProviderCredentialEvents>,
    ) -> Result<()> {
        Ok(())
    }
    fn UnAdvise(&self) -> Result<()> {
        Ok(())
    }
    fn SetSelected(&self) -> Result<BOOL> {
        Ok(BOOL(1))
    }
    fn SetDeselected(&self) -> Result<()> {
        Ok(())
    }
    fn GetFieldState(
        &self,
        _dwFieldID: u32,
        pcpfs: *mut CREDENTIAL_PROVIDER_FIELD_STATE,
        pcpfis: *mut CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
    ) -> Result<()> {
        unsafe {
            *pcpfs = CPFS_HIDDEN;
            *pcpfis = CPFIS_NONE;
        }
        Ok(())
    }
    fn GetStringValue(&self, _dwFieldID: u32) -> Result<PWSTR> {
        Ok(PWSTR::null())
    }
    fn GetBitmapValue(&self, _dwFieldID: u32) -> Result<HBITMAP> {
        Err(E_NOTIMPL.into())
    }
    fn GetCheckboxValue(
        &self,
        _dwFieldID: u32,
        _pbChecked: *mut BOOL,
        _ppszLabel: *mut PWSTR,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetSubmitButtonValue(&self, _dwFieldID: u32) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn GetComboBoxValueCount(
        &self,
        _dwFieldID: u32,
        _pcItems: *mut u32,
        _pdwSelectedItem: *mut u32,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetComboBoxValueAt(&self, _dwFieldID: u32, _dwItem: u32) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }
    fn SetStringValue(&self, _dwFieldID: u32, _psz: &PCWSTR) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetCheckboxValue(&self, _dwFieldID: u32, _bChecked: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetComboBoxSelectedValue(&self, _dwFieldID: u32, _dwSelectedItem: u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn CommandLinkClicked(&self, _dwFieldID: u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn ReportResult(
        &self,
        _ntsStatus: NTSTATUS,
        _ntsSubstatus: NTSTATUS,
        _ppszOptionalStatusText: *mut PWSTR,
        _pcpsiOptionalStatusIcon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
    ) -> Result<()> {
        Ok(())
    }

    fn GetSerialization(
        &self,
        pcpgsr: *mut CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE,
        pcpcs: *mut CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
        ppszOptionalStatusText: *mut PWSTR,
        pcpsiOptionalStatusIcon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
    ) -> Result<()> {
        unsafe {
            *ppszOptionalStatusText = PWSTR::null();
            *pcpsiOptionalStatusIcon = CPSI_NONE;
            ptr::write_bytes(pcpcs, 0, 1);
            *pcpgsr = CPGSR_NO_CREDENTIAL_NOT_FINISHED;
        }

        let (domain, user) = match read_user_from_file() {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };

        let (buf, cb, auth_pkg) = pack_s4u(&domain, &user)?;

        unsafe {
            (*pcpcs).ulAuthenticationPackage = auth_pkg;
            (*pcpcs).clsidCredentialProvider = PHANTOM_CP_CLSID;
            (*pcpcs).cbSerialization = cb;
            (*pcpcs).rgbSerialization = buf;
            *pcpgsr = CPGSR_RETURN_CREDENTIAL_FINISHED;
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn alloc_wide(s: &str) -> PWSTR {
    let v: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let p = CoTaskMemAlloc(v.len() * 2) as *mut u16;
        ptr::copy_nonoverlapping(v.as_ptr(), p, v.len());
        PWSTR(p)
    }
}

/// Read + consume the auth ticket. Format: first line is `user` or
/// `domain\user`. Any trailing `:<password>` after the user is ignored
/// (legacy Phase-1 format). Returns (domain, user).
fn read_user_from_file() -> std::result::Result<(String, String), std::io::Error> {
    let meta = std::fs::metadata(AUTH_FILE)?;
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = std::time::SystemTime::now().duration_since(modified) {
            if age.as_secs() > TICKET_MAX_AGE_SECS {
                let _ = std::fs::remove_file(AUTH_FILE);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "ticket stale",
                ));
            }
        }
    }

    let raw = std::fs::read_to_string(AUTH_FILE)?;
    let line = raw.lines().next().unwrap_or("").trim();
    // Tolerate legacy `user:pass` by dropping the pass half.
    let user_part = line.split_once(':').map(|(u, _)| u).unwrap_or(line);
    let (domain, user) = match user_part.split_once('\\') {
        Some((d, u)) => (d.to_string(), u.to_string()),
        None => (String::new(), user_part.to_string()),
    };
    if user.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty user in ticket",
        ));
    }
    // Single-use: burn the ticket now.
    let _ = std::fs::remove_file(AUTH_FILE);
    Ok((domain, user))
}

// MSV1_0_S4U_LOGON struct — not in windows-rs 0.58, declared by hand.
// https://learn.microsoft.com/en-us/windows/win32/api/ntsecapi/ns-ntsecapi-msv1_0_s4u_logon
#[repr(C)]
struct Msv10S4uLogon {
    message_type: u32, // MsV1_0S4ULogon = 12
    flags: u32,
    user_principal_name: UNICODE_STRING,
    domain_name: UNICODE_STRING,
}
const MSV1_0_S4U_LOGON_TYPE: u32 = 12;

/// Build an `MSV1_0_S4U_LOGON` serialization that LSA (called by LogonUI
/// with SeTcbPrivilege) will accept as a passwordless logon for a local
/// account. UNICODE_STRING.Buffer fields are offsets-from-start.
fn pack_s4u(domain: &str, user: &str) -> Result<(*mut u8, u32, u32)> {
    let u_w: Vec<u16> = user.encode_utf16().collect();
    let d_w: Vec<u16> = domain.encode_utf16().collect();

    let u_bytes = u_w.len() * 2;
    let d_bytes = d_w.len() * 2;

    let base_size = mem::size_of::<Msv10S4uLogon>();
    let total = base_size + u_bytes + d_bytes;

    let buf = unsafe { CoTaskMemAlloc(total) as *mut u8 };
    if buf.is_null() {
        return Err(E_POINTER.into());
    }
    unsafe {
        ptr::write_bytes(buf, 0, total);
    }

    let u_off = base_size;
    let d_off = u_off + u_bytes;

    unsafe {
        if u_bytes > 0 {
            ptr::copy_nonoverlapping(u_w.as_ptr() as *const u8, buf.add(u_off), u_bytes);
        }
        if d_bytes > 0 {
            ptr::copy_nonoverlapping(d_w.as_ptr() as *const u8, buf.add(d_off), d_bytes);
        }

        let s4u = buf as *mut Msv10S4uLogon;
        (*s4u).message_type = MSV1_0_S4U_LOGON_TYPE;
        (*s4u).flags = 0;
        (*s4u).user_principal_name.Length = u_bytes as u16;
        (*s4u).user_principal_name.MaximumLength = u_bytes as u16;
        (*s4u).user_principal_name.Buffer = PWSTR(u_off as *mut u16);
        (*s4u).domain_name.Length = d_bytes as u16;
        (*s4u).domain_name.MaximumLength = d_bytes as u16;
        (*s4u).domain_name.Buffer = PWSTR(d_off as *mut u16);
    }

    let auth_pkg = lookup_auth_package("MICROSOFT_AUTHENTICATION_PACKAGE_V1_0")?;
    Ok((buf, total as u32, auth_pkg))
}

fn lookup_auth_package(name: &str) -> Result<u32> {
    use windows::Win32::Security::Authentication::Identity::{
        LsaConnectUntrusted, LsaDeregisterLogonProcess, LsaLookupAuthenticationPackage,
        LSA_STRING,
    };

    unsafe {
        let mut lsa: HANDLE = HANDLE::default();
        LsaConnectUntrusted(&mut lsa).ok()?;

        let name_bytes = name.as_bytes();
        let name_str = LSA_STRING {
            Length: name_bytes.len() as u16,
            MaximumLength: name_bytes.len() as u16,
            Buffer: windows::core::PSTR(name_bytes.as_ptr() as *mut u8),
        };

        let mut pkg: u32 = 0;
        let status = LsaLookupAuthenticationPackage(lsa, &name_str, &mut pkg);
        let _ = LsaDeregisterLogonProcess(lsa);
        status.ok()?;
        Ok(pkg)
    }
}
