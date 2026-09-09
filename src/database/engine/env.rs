use std::{
	ptr::eq as ptr_eq,
	sync::{Arc, LockResult, Mutex, MutexGuard, PoisonError, Weak},
};

use tuwunel_core::{Result, Server, debug, implement};

use crate::or_else;

/// The shared rocksdb environment.
///
/// The inner mutex guards the handle itself, which the engine needs while
/// opening a database or a backup. Releasing the last reference shuts down and
/// joins the environment's background threads, so an engine must hold one for
/// as long as it is open.
pub(super) struct Env(Mutex<rocksdb::Env>);

/// The process-global rocksdb environment, held weakly so it lives exactly as
/// long as some context needs it.
///
/// `Env::new` returns rocksdb's default environment singleton, so every handle
/// addresses the same object and its thread pools are shared by every engine
/// open in this process. Locking this slot across both acquisition and
/// teardown is what keeps the two mutually exclusive.
static ENV: Mutex<Weak<Env>> = Mutex::new(Weak::new());

/// Take a reference to the shared environment, creating one when no context
/// currently holds it.
///
/// The priority knobs apply to the environment rather than to any one context,
/// so they are read from the config of whichever server first needs it.
#[implement(Env)]
pub(super) fn acquire(server: &Server) -> Result<Arc<Self>> {
	Self::acquire_with_priorities(
		server.config.rocksdb_compaction_prio_idle,
		server.config.rocksdb_compaction_ioprio_idle,
	)
}

/// Take a shared environment reference with the requested idle priorities.
///
/// The slot is held throughout acquisition so it cannot interleave with
/// teardown; priorities apply only when creating the shared handle.
#[implement(Env)]
pub(super) fn acquire_with_priorities(cpu_idle: bool, io_idle: bool) -> Result<Arc<Self>> {
	let mut slot = ENV.lock().expect("environment slot locked");

	if let Some(env) = slot.upgrade() {
		return Ok(env);
	}

	let mut env = rocksdb::Env::new().or_else(or_else)?;

	if cpu_idle {
		env.lower_thread_pool_cpu_priority();
	}

	if io_idle {
		env.lower_thread_pool_io_priority();
	}

	let env = Arc::new(Self(env.into()));
	*slot = Arc::downgrade(&env);

	Ok(env)
}

#[implement(Env)]
#[inline]
pub(super) fn lock(&self) -> LockResult<MutexGuard<'_, rocksdb::Env>> { self.0.lock() }

impl Drop for Env {
	#[cold]
	fn drop(&mut self) {
		let mut slot = ENV.lock().expect("environment slot locked");

		// A context which acquired after our last strong reference went away
		// owns the same environment now, so the shutdown is its job.
		if !ptr_eq(slot.as_ptr(), self) {
			return;
		}

		*slot = Weak::new();

		let env = self
			.0
			.get_mut()
			.unwrap_or_else(PoisonError::into_inner);

		debug!("Shutting down background threads");
		env.set_high_priority_background_threads(0);
		env.set_low_priority_background_threads(0);
		env.set_bottom_priority_background_threads(0);
		env.set_background_threads(0);

		debug!("Joining background threads...");
		env.join_all_threads();
	}
}
