//! A shared library the workspace image ships, opened at run time. Nothing
//! links against it, so a build or platform without it is unaffected: the
//! feature it backs reports itself unavailable.

use std::ffi::{c_void, CStr};
use std::ptr::NonNull;

pub(crate) struct Library {
    handle: NonNull<c_void>,
}

// SAFETY: a dlopen handle is process-global and may be used from any thread.
unsafe impl Send for Library {}
// SAFETY: as above; `symbol` only reads through the handle.
unsafe impl Sync for Library {}

impl Library {
    /// Opens `soname` with its symbols resolved now and kept private to
    /// this handle. The library is never closed: function pointers taken
    /// from it may live for the rest of the process.
    pub(crate) fn open(soname: &CStr) -> Result<Self, String> {
        // SAFETY: `soname` is NUL-terminated; dlopen has no other input.
        let handle = unsafe { libc::dlopen(soname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        NonNull::new(handle)
            .map(|handle| Self { handle })
            .ok_or_else(|| format!("{}: {}", soname.to_string_lossy(), last_error()))
    }

    /// The function `name` exports.
    ///
    /// # Safety
    /// `F` must be the exact `extern "C"` function pointer type of `name`.
    pub(crate) unsafe fn function<F: Copy>(&self, name: &CStr) -> Result<F, String> {
        const {
            assert!(std::mem::size_of::<F>() == std::mem::size_of::<*mut c_void>());
        }
        // SAFETY: the handle is open for the process lifetime and `name` is
        // NUL-terminated.
        let address = unsafe { libc::dlsym(self.handle.as_ptr(), name.as_ptr()) };
        if address.is_null() {
            return Err(format!("{}: {}", name.to_string_lossy(), last_error()));
        }
        // SAFETY: the caller names the symbol's exact pointer type, which has
        // the size of a data pointer (checked above).
        Ok(unsafe { std::mem::transmute_copy(&address) })
    }
}

fn last_error() -> String {
    // SAFETY: dlerror returns a NUL-terminated thread-local message or null.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "not found".into();
    }
    // SAFETY: non-null, NUL-terminated and valid until the next dl* call on
    // this thread; copied at once.
    unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_library_and_symbol_are_errors_not_crashes() {
        let missing = Library::open(c"libambit-does-not-exist.so.0")
            .err()
            .unwrap();
        assert!(missing.contains("libambit-does-not-exist"), "{missing}");
        let libc = Library::open(c"libc.so.6").unwrap();
        type Getpid = unsafe extern "C" fn() -> libc::pid_t;
        // SAFETY: getpid has exactly this signature.
        let getpid: Getpid = unsafe { libc.function(c"getpid") }.unwrap();
        // SAFETY: getpid has no preconditions.
        assert_eq!(unsafe { getpid() } as u32, std::process::id());
        // SAFETY: never called.
        let absent = unsafe { libc.function::<Getpid>(c"ambit_no_such_symbol") };
        assert!(absent.is_err());
    }
}
