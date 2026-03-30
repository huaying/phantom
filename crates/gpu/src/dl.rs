//! Runtime dynamic library loading (dlopen/dlsym/dlclose).

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
            let handle = unsafe { libc_dlopen(c_name.as_ptr(), RTLD_LAZY) };
            if !handle.is_null() {
                tracing::debug!("loaded {name}");
                return Ok(Self { handle });
            }
        }
        let err = unsafe { dlerror_str() };
        anyhow::bail!("failed to load {:?}: {err}", names)
    }

    /// Resolve a symbol to a function pointer.
    ///
    /// # Safety
    /// Caller must ensure the symbol has the correct type `T`.
    pub unsafe fn sym<T>(&self, name: &str) -> anyhow::Result<T> {
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        let c_name = CString::new(name).unwrap();
        let ptr = unsafe { libc_dlsym(self.handle, c_name.as_ptr()) };
        if ptr.is_null() {
            let err = unsafe { dlerror_str() };
            anyhow::bail!("symbol '{name}' not found: {err}");
        }
        Ok(unsafe { std::mem::transmute_copy(&ptr) })
    }
}

impl Drop for DynLib {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { libc_dlclose(self.handle) };
        }
    }
}

const RTLD_LAZY: i32 = 1;

unsafe fn dlerror_str() -> String {
    let ptr = unsafe { libc_dlerror() };
    if ptr.is_null() {
        "unknown error".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

extern "C" {
    #[link_name = "dlopen"]
    fn libc_dlopen(filename: *const c_char, flags: i32) -> *mut c_void;
    #[link_name = "dlsym"]
    fn libc_dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    #[link_name = "dlclose"]
    fn libc_dlclose(handle: *mut c_void) -> i32;
    #[link_name = "dlerror"]
    fn libc_dlerror() -> *const c_char;
}
