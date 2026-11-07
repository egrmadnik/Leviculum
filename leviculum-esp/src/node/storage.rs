//! The internal-flash handle, shared between the main loop and the tasks.
//!
//! `esp-storage`'s `FlashStorage` is one object; the PID app saves its
//! config from the loop and the Blink app's persist task writes from its
//! own Embassy task, so the object lives behind a blocking mutex in a
//! static. Every flash operation is a short synchronous call made inside
//! the lock.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use esp_storage::FlashStorage;
use static_cell::StaticCell;

/// The one flash handle.
pub type SharedFlash = Mutex<CriticalSectionRawMutex, RefCell<FlashStorage<'static>>>;

/// Move the flash handle into its static and hand back the reference.
/// Call once, after the identity has been loaded.
pub fn share(flash: FlashStorage<'static>) -> &'static SharedFlash {
    static CELL: StaticCell<SharedFlash> = StaticCell::new();
    CELL.init(Mutex::new(RefCell::new(flash)))
}

/// Run `f` with exclusive access to the flash.
pub fn with<R>(shared: &SharedFlash, f: impl FnOnce(&mut FlashStorage<'static>) -> R) -> R {
    shared.lock(|cell| f(&mut cell.borrow_mut()))
}
