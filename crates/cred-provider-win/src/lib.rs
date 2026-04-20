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
use windows::Win32::Security::Authentication::Identity::{
    KerbInteractiveLogon, KerbWorkstationUnlockLogon, KERB_INTERACTIVE_UNLOCK_LOGON,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, IClassFactory, IClassFactory_Impl};
use windows::Win32::UI::Shell::*;

// ccd145e9-71bb-4e91-a604-2ee449adfd54
pub const PHANTOM_CP_CLSID: GUID = GUID::from_u128(0xccd145e9_71bb_4e91_a604_2ee449adfd54);

const AUTH_FILE: &str = r"C:\ProgramData\phantom\auth";

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

        let (domain, user, pass) = match read_creds_from_file() {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };

        let cpus = CPUS_STATE.load(Ordering::SeqCst);
        let (buf, cb, auth_pkg) = pack_kiul(&domain, &user, &pass, cpus)?;

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

fn read_creds_from_file() -> std::result::Result<(String, String, String), std::io::Error> {
    let raw = std::fs::read_to_string(AUTH_FILE)?;
    let line = raw.lines().next().unwrap_or("").trim();
    let (userpart, pass) = line.split_once(':').ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing ':' in auth file")
    })?;
    let (domain, user) = match userpart.split_once('\\') {
        Some((d, u)) => (d.to_string(), u.to_string()),
        None => (String::new(), userpart.to_string()),
    };
    Ok((domain, user, pass.to_string()))
}

fn pack_kiul(
    domain: &str,
    user: &str,
    pass: &str,
    cpus: u32,
) -> Result<(*mut u8, u32, u32)> {
    let d_w: Vec<u16> = domain.encode_utf16().collect();
    let u_w: Vec<u16> = user.encode_utf16().collect();
    let p_w: Vec<u16> = pass.encode_utf16().collect();

    let d_bytes = d_w.len() * 2;
    let u_bytes = u_w.len() * 2;
    let p_bytes = p_w.len() * 2;

    let base_size = mem::size_of::<KERB_INTERACTIVE_UNLOCK_LOGON>();
    let total = base_size + d_bytes + u_bytes + p_bytes;

    let buf = unsafe { CoTaskMemAlloc(total) as *mut u8 };
    if buf.is_null() {
        return Err(E_POINTER.into());
    }
    unsafe {
        ptr::write_bytes(buf, 0, total);
    }

    let d_off = base_size;
    let u_off = d_off + d_bytes;
    let p_off = u_off + u_bytes;

    unsafe {
        if d_bytes > 0 {
            ptr::copy_nonoverlapping(d_w.as_ptr() as *const u8, buf.add(d_off), d_bytes);
        }
        if u_bytes > 0 {
            ptr::copy_nonoverlapping(u_w.as_ptr() as *const u8, buf.add(u_off), u_bytes);
        }
        if p_bytes > 0 {
            ptr::copy_nonoverlapping(p_w.as_ptr() as *const u8, buf.add(p_off), p_bytes);
        }

        let kiul = buf as *mut KERB_INTERACTIVE_UNLOCK_LOGON;
        (*kiul).Logon.MessageType = if cpus == CPUS_UNLOCK_WORKSTATION.0 as u32 {
            KerbWorkstationUnlockLogon
        } else {
            KerbInteractiveLogon
        };
        (*kiul).Logon.LogonDomainName.Length = d_bytes as u16;
        (*kiul).Logon.LogonDomainName.MaximumLength = d_bytes as u16;
        (*kiul).Logon.LogonDomainName.Buffer = PWSTR(d_off as *mut u16);
        (*kiul).Logon.UserName.Length = u_bytes as u16;
        (*kiul).Logon.UserName.MaximumLength = u_bytes as u16;
        (*kiul).Logon.UserName.Buffer = PWSTR(u_off as *mut u16);
        (*kiul).Logon.Password.Length = p_bytes as u16;
        (*kiul).Logon.Password.MaximumLength = p_bytes as u16;
        (*kiul).Logon.Password.Buffer = PWSTR(p_off as *mut u16);
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
