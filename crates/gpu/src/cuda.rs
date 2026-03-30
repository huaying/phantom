//! CUDA driver API wrapper — context creation and device memory management.

use crate::dl::DynLib;
use crate::sys::*;
use anyhow::{bail, Context, Result};
use std::ffi::c_void;

type FnCuInit = unsafe extern "C" fn(flags: u32) -> CUresult;
type FnCuDeviceGet = unsafe extern "C" fn(device: *mut CUdevice, ordinal: i32) -> CUresult;
type FnCuCtxCreate = unsafe extern "C" fn(ctx: *mut CUcontext, flags: u32, dev: CUdevice) -> CUresult;
type FnCuCtxDestroy = unsafe extern "C" fn(ctx: CUcontext) -> CUresult;
type FnCuCtxPushCurrent = unsafe extern "C" fn(ctx: CUcontext) -> CUresult;
type FnCuCtxPopCurrent = unsafe extern "C" fn(ctx: *mut CUcontext) -> CUresult;
type FnCuMemAlloc = unsafe extern "C" fn(ptr: *mut CUdeviceptr, size: usize) -> CUresult;
type FnCuMemFree = unsafe extern "C" fn(ptr: CUdeviceptr) -> CUresult;
type FnCuMemcpyHtoD = unsafe extern "C" fn(dst: CUdeviceptr, src: *const c_void, size: usize) -> CUresult;
type FnCuMemcpyDtoH = unsafe extern "C" fn(dst: *mut c_void, src: CUdeviceptr, size: usize) -> CUresult;

pub struct CudaLib {
    _lib: DynLib,
    _cu_init: FnCuInit,
    cu_device_get: FnCuDeviceGet,
    cu_ctx_create: FnCuCtxCreate,
    cu_ctx_destroy: FnCuCtxDestroy,
    cu_ctx_push: FnCuCtxPushCurrent,
    cu_ctx_pop: FnCuCtxPopCurrent,
    cu_mem_alloc: FnCuMemAlloc,
    cu_mem_free: FnCuMemFree,
    cu_memcpy_htod: FnCuMemcpyHtoD,
    cu_memcpy_dtoh: FnCuMemcpyDtoH,
}

impl CudaLib {
    pub fn load() -> Result<Self> {
        let lib = DynLib::open(&["libcuda.so.1", "libcuda.so"])
            .context("failed to load CUDA driver library")?;

        unsafe {
            let cu_init: FnCuInit = lib.sym("cuInit").context("cuInit")?;
            let cu_device_get: FnCuDeviceGet = lib.sym("cuDeviceGet").context("cuDeviceGet")?;
            let cu_ctx_create: FnCuCtxCreate = lib.sym("cuCtxCreate_v2").context("cuCtxCreate_v2")?;
            let cu_ctx_destroy: FnCuCtxDestroy = lib.sym("cuCtxDestroy_v2").context("cuCtxDestroy_v2")?;
            let cu_ctx_push: FnCuCtxPushCurrent = lib.sym("cuCtxPushCurrent_v2").context("cuCtxPushCurrent_v2")?;
            let cu_ctx_pop: FnCuCtxPopCurrent = lib.sym("cuCtxPopCurrent_v2").context("cuCtxPopCurrent_v2")?;
            let cu_mem_alloc: FnCuMemAlloc = lib.sym("cuMemAlloc_v2").context("cuMemAlloc_v2")?;
            let cu_mem_free: FnCuMemFree = lib.sym("cuMemFree_v2").context("cuMemFree_v2")?;
            let cu_memcpy_htod: FnCuMemcpyHtoD = lib.sym("cuMemcpyHtoD_v2").context("cuMemcpyHtoD_v2")?;
            let cu_memcpy_dtoh: FnCuMemcpyDtoH = lib.sym("cuMemcpyDtoH_v2").context("cuMemcpyDtoH_v2")?;

            let status = (cu_init)(0);
            if status != CUDA_SUCCESS {
                bail!("cuInit failed: {status}");
            }

            Ok(Self {
                _lib: lib,
                _cu_init: cu_init,
                cu_device_get,
                cu_ctx_create,
                cu_ctx_destroy,
                cu_ctx_push,
                cu_ctx_pop,
                cu_mem_alloc,
                cu_mem_free,
                cu_memcpy_htod,
                cu_memcpy_dtoh,
            })
        }
    }

    pub fn device_get(&self, ordinal: i32) -> Result<CUdevice> {
        let mut dev: CUdevice = 0;
        let status = unsafe { (self.cu_device_get)(&mut dev, ordinal) };
        if status != CUDA_SUCCESS {
            bail!("cuDeviceGet({ordinal}) failed: {status}");
        }
        Ok(dev)
    }

    pub fn ctx_create(&self, dev: CUdevice) -> Result<CUcontext> {
        let mut ctx: CUcontext = std::ptr::null_mut();
        let status = unsafe { (self.cu_ctx_create)(&mut ctx, 0, dev) };
        if status != CUDA_SUCCESS {
            bail!("cuCtxCreate failed: {status}");
        }
        Ok(ctx)
    }

    /// # Safety
    /// `ctx` must be a valid CUDA context returned by `ctx_create`.
    pub unsafe fn ctx_destroy(&self, ctx: CUcontext) {
        unsafe { (self.cu_ctx_destroy)(ctx) };
    }

    /// # Safety
    /// `ctx` must be a valid CUDA context.
    pub unsafe fn ctx_push(&self, ctx: CUcontext) -> Result<()> {
        let status = unsafe { (self.cu_ctx_push)(ctx) };
        if status != CUDA_SUCCESS {
            bail!("cuCtxPushCurrent failed: {status}");
        }
        Ok(())
    }

    pub fn ctx_pop(&self) -> Result<()> {
        let mut old: CUcontext = std::ptr::null_mut();
        let status = unsafe { (self.cu_ctx_pop)(&mut old) };
        if status != CUDA_SUCCESS {
            bail!("cuCtxPopCurrent failed: {status}");
        }
        Ok(())
    }

    pub fn mem_alloc(&self, size: usize) -> Result<CUdeviceptr> {
        let mut ptr: CUdeviceptr = 0;
        let status = unsafe { (self.cu_mem_alloc)(&mut ptr, size) };
        if status != CUDA_SUCCESS {
            bail!("cuMemAlloc({size}) failed: {status}");
        }
        Ok(ptr)
    }

    pub fn mem_free(&self, ptr: CUdeviceptr) {
        unsafe { (self.cu_mem_free)(ptr) };
    }

    pub fn memcpy_htod(&self, dst: CUdeviceptr, src: &[u8]) -> Result<()> {
        let status = unsafe {
            (self.cu_memcpy_htod)(dst, src.as_ptr() as *const c_void, src.len())
        };
        if status != CUDA_SUCCESS {
            bail!("cuMemcpyHtoD failed: {status}");
        }
        Ok(())
    }

    pub fn memcpy_dtoh(&self, dst: &mut [u8], src: CUdeviceptr) -> Result<()> {
        let status = unsafe {
            (self.cu_memcpy_dtoh)(dst.as_mut_ptr() as *mut c_void, src, dst.len())
        };
        if status != CUDA_SUCCESS {
            bail!("cuMemcpyDtoH failed: {status}");
        }
        Ok(())
    }
}
