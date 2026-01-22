use std::sync::atomic::{AtomicI32, Ordering};

const ORDERING: Ordering = Ordering::Relaxed;
static STATIC_DTORS: AtomicI32 = AtomicI32::new(0);

/// Called by Module::unload() to indicate module is about to be unloaded and
/// static destruction is intended. This will allow verifying it actually took
/// place.
pub(crate) fn prepare() {
	let count = STATIC_DTORS.fetch_sub(1, ORDERING);
	debug_assert!(count <= 0, "STATIC_DTORS should not be greater than zero.");
}

/// Called by static destructor of a module. This call should only be found
/// inside a mod_fini! macro. Do not call from anywhere else.
#[inline(always)]
pub fn report() { let _count = STATIC_DTORS.fetch_add(1, ORDERING); }

/// Called by Module::unload() (see check()) with action in case a check()
/// failed. This can allow a stuck module to be noted while allowing for other
/// independent modules to be diagnosed.
pub(crate) fn check_and_reset() -> bool { STATIC_DTORS.swap(0, ORDERING) == 0 }

/// Called by Module::unload() after unload to verify static destruction took
/// place. A call to prepare() must be made prior to Module::unload() and making
/// this call.
#[expect(dead_code)]
pub(crate) fn check() -> bool { STATIC_DTORS.load(ORDERING) == 0 }
