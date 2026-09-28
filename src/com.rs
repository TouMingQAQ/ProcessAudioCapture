//! COM apartment helper shared by the enumeration entry points.
//!
//! The capture worker initialises COM on its own thread; the enumeration
//! functions are called from whatever thread the host happens to use, so they
//! have to make sure the apartment exists there too.

use windows::Win32::{
    Foundation::RPC_E_CHANGED_MODE,
    System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
};

/// Initialises a multithreaded apartment for the current thread and tears it
/// down again on drop.
///
/// `RPC_E_CHANGED_MODE` means the thread already lives in an apartment (an STA,
/// typically because the caller is a GUI thread). COM works there as well, so
/// that is not an error - it only means this guard must not uninitialise.
pub(crate) struct ComGuard {
    owned: bool,
}

impl ComGuard {
    pub(crate) fn new() -> Result<Self, ()> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() {
            return Ok(Self { owned: true });
        }
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self { owned: false });
        }
        Err(())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}
