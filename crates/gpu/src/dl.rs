//! Runtime dynamic library loading.
//! Unix: dlopen/dlsym/dlclose
//! Windows: LoadLibraryA/GetProcAddress/FreeLibrary

use std::ffi::{c_char, c_void, CString};

pub struct DynLib {
    handle: *mut c_void,
}

unsafe impl Send for DynLib {}
unsafe impl Sync for DynLib {}

impl DynLib {
    /// Open a shared library by name. Tries each name in order.
    pub fn open(names: &[&str]) -> anyhow::Result<Self> {
        for name in names {
            let c_name = CString::new(*name).unwrap();
            let handle = unsafe { lib_open(c_name.as_ptr()) };
            if !handle.is_null() {
                tracing::debug!("loaded {name}");
                return Ok(Self { handle });
            }
        }
        let err = unsafe { last_error() };
        anyhow::bail!("failed to load {:?}: {err}", names)
    }

    /// Resolve a symbol to a function pointer.
    ///
    /// # Safety
    /// Caller must ensure the symbol has the correct type `T`.
    pub unsafe fn sym<T>(&self, name: &str) -> anyhow::Result<T> {
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        let c_name = CString::new(name).unwrap();
        let ptr = unsafe { lib_sym(self.handle, c_name.as_ptr()) };
        if ptr.is_null() {
            let err = unsafe { last_error() };
            anyhow::bail!("symbol '{name}' not found: {err}");
        }
        Ok(unsafe { std::mem::transmute_copy(&ptr) })
    }
}

impl Drop for DynLib {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { lib_close(self.handle) };
        }
    }
}

// --- Unix (dlopen) ---

#[cfg(unix)]
unsafe fn lib_open(name: *const c_char) -> *mut c_void {
    const RTLD_LAZY: i32 = 1;
    dlopen(name, RTLD_LAZY)
}

#[cfg(unix)]
unsafe fn lib_sym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    dlsym(handle, name)
}

#[cfg(unix)]
unsafe fn lib_close(handle: *mut c_void) {
    dlclose(handle);
}

#[cfg(unix)]
unsafe fn last_error() -> String {
    let ptr = dlerror();
    if ptr.is_null() {
        "unknown error".to_string()
    } else {
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

#[cfg(unix)]
extern "C" {
    fn dlopen(filename: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> i32;
    fn dlerror() -> *const c_char;
}

// --- Windows (LoadLibrary) ---

#[cfg(windows)]
unsafe fn lib_open(name: *const c_char) -> *mut c_void {
    LoadLibraryA(name)
}

#[cfg(windows)]
unsafe fn lib_sym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    GetProcAddress(handle, name)
}

#[cfg(windows)]
unsafe fn lib_close(handle: *mut c_void) {
    FreeLibrary(handle);
}

#[cfg(windows)]
unsafe fn last_error() -> String {
    let code = GetLastError();
    format!("Win32 error {code}")
}

#[cfg(windows)]
extern "system" {
    fn LoadLibraryA(lpFileName: *const c_char) -> *mut c_void;
    fn GetProcAddress(hModule: *mut c_void, lpProcName: *const c_char) -> *mut c_void;
    fn FreeLibrary(hModule: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}
