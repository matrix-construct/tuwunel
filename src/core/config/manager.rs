use std::{
	cell::{Cell, RefCell},
	ops::Deref,
	ptr,
	ptr::null_mut,
	sync::{
		Arc,
		atomic::{AtomicPtr, Ordering},
	},
};

use super::Config;
use crate::{Result, implement};

/// The configuration manager is an indirection to reload the configuration for
/// the server while it is running. In order to not burden or clutter the many
/// callsites which query for configuration items, this object implements Deref
/// for the actively loaded configuration.
pub struct Manager {
	active: AtomicPtr<Config>,
}

thread_local! {
	static INDEX: Cell<usize> = const { Cell::new(0_usize) };
	static HANDLE: RefCell<Handles> = const {
		RefCell::new([const { None }; HISTORY])
	};
}

type Handle = Option<Arc<Config>>;
type Handles = [Handle; HISTORY];

const HISTORY: usize = 8;

impl Manager {
	pub(crate) fn new(config: Config) -> Self {
		let config = Arc::new(config);
		Self {
			active: AtomicPtr::new(Arc::into_raw(config).cast_mut()),
		}
	}
}

impl Drop for Manager {
	fn drop(&mut self) {
		let config = self.active.swap(null_mut(), Ordering::AcqRel);

		// SAFETY: The active pointer was set using an Arc::into_raw(). We're obliged to
		// reconstitute that into Arc otherwise it will leak.
		unsafe { Arc::from_raw(config) };
	}
}

impl Deref for Manager {
	type Target = Arc<Config>;

	fn deref(&self) -> &Self::Target { HANDLE.with_borrow_mut(|handle| self.load(handle)) }
}

/// Update the active configuration, returning prior configuration.
#[implement(Manager)]
#[tracing::instrument(skip_all)]
pub fn update(&self, config: Config) -> Result<Arc<Config>> {
	let config = Arc::new(config);
	let new = Arc::into_raw(config);
	let old = self.active.swap(new.cast_mut(), Ordering::AcqRel);

	// SAFETY: The old active pointer was set using an Arc::into_raw(). We're
	// obliged to reconstitute that into Arc otherwise it will leak.
	Ok(unsafe { Arc::from_raw(old) })
}

#[implement(Manager)]
fn load(&self, handle: &mut [Option<Arc<Config>>]) -> &'static Arc<Config> {
	let config = self.active.load(Ordering::Acquire);

	// Branch taken after config reload or first access by this thread.
	if handle[INDEX.get()]
		.as_ref()
		.is_none_or(|handle| !ptr::eq(config, Arc::as_ptr(handle)))
	{
		INDEX.set(INDEX.get().wrapping_add(1).wrapping_rem(HISTORY));
		return load_miss(handle, INDEX.get(), config);
	}

	let config: &Arc<Config> = handle[INDEX.get()]
		.as_ref()
		.expect("handle was already cached for this thread");

	// SAFETY: The caller should not hold multiple references at a time directly
	// into Config, as a subsequent reference might invalidate the thread's cache
	// causing another reference to dangle.
	//
	// This is a highly unusual pattern as most config values are copied by value or
	// used immediately without running overlap with another value. Even if it does
	// actually occur somewhere, the window of danger is limited to the config being
	// reloaded while the reference is held and another access is made by the same
	// thread into a different config value. This is mitigated by creating a buffer
	// of old configs rather than discarding at the earliest opportunity; the odds
	// of this scenario are thus astronomical.
	unsafe { std::mem::transmute(config) }
}

#[tracing::instrument(
	name = "miss",
	level = "trace",
	skip_all,
	fields(%index, ?config)
)]
#[expect(clippy::transmute_ptr_to_ptr)]
fn load_miss(
	handle: &mut [Option<Arc<Config>>],
	index: usize,
	config: *const Config,
) -> &'static Arc<Config> {
	// SAFETY: The active pointer was set prior and always remains valid. We're
	// reconstituting the Arc here but as a new reference, so the count is
	// incremented. This instance will be cached in the thread-local.
	let config = unsafe {
		Arc::increment_strong_count(config);
		Arc::from_raw(config)
	};

	// SAFETY: See the note on the transmute above. The caller should not hold more
	// than one reference at a time directly into Config, as the second access
	// might invalidate the thread's cache, dangling the reference to the first.
	unsafe { std::mem::transmute(handle[index].insert(config)) }
}
