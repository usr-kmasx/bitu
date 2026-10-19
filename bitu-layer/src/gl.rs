//! Hook OpenGL do Bitu (via LD_PRELOAD): interessa glXSwapBuffers e eglSwapBuffers.
//! A resolução do símbolo real usa dlsym(RTLD_NEXT) com fallback dlopen.

use crate::pacing;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

type PfnGlxSwap = unsafe extern "C" fn(*mut c_void, usize);
type PfnEglSwap = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;

fn cached(name: &'static [u8], slot: &AtomicUsize, lib: &[&str]) -> usize {
    let mut p = slot.load(Ordering::Acquire);
    if p != 0 {
        return p;
    }
    p = unsafe { lookup_symbol(name, lib) };
    if p != 0 {
        slot.store(p, Ordering::Release);
    }
    p
}

/// dlsym(RTLD_NEXT) + fallback dlopen das libs típicas.
unsafe fn lookup_symbol(name: &[u8], libs: &[&str]) -> usize {
    // nome deve ser NUL-terminado: passamos literais com \0
    let p = libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const libc::c_char) as usize;
    if p != 0 {
        return p;
    }
    for lib in libs {
        let cname = std::ffi::CString::new(*lib).unwrap();
        let handle = libc::dlopen(cname.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if handle.is_null() {
            continue;
        }
        let p = libc::dlsym(handle, name.as_ptr() as *const libc::c_char) as usize;
        libc::dlclose(handle);
        if p != 0 {
            return p;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn glXSwapBuffers(dpy: *mut c_void, drawable: usize) {
    pacing::frame();
    let p = cached(b"glXSwapBuffers\0", &GLX_REAL, &["libGL.so.1", "libGLX.so.0"]);
    if p != 0 {
        let f: PfnGlxSwap = std::mem::transmute(p);
        f(dpy, drawable);
    }
}

#[no_mangle]
pub unsafe extern "C" fn eglSwapBuffers(dpy: *mut c_void, surface: *mut c_void) -> u32 {
    pacing::frame();
    let p = cached(
        b"eglSwapBuffers\0",
        &EGL_REAL,
        &["libEGL.so.1", "libGL.so.1"],
    );
    if p != 0 {
        let f: PfnEglSwap = std::mem::transmute(p);
        return f(dpy, surface);
    }
    1 // EGL_TRUE
}

static GLX_REAL: AtomicUsize = AtomicUsize::new(0);
static EGL_REAL: AtomicUsize = AtomicUsize::new(0);
