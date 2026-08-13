#[cfg(test)]
mod tests;

use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::SystemTime,
};

use ruma::OwnedServerName;
use tokio::{runtime, sync::broadcast};

use crate::{Err, Result, config, config::Config, log::Logging, metrics::Metrics};

/// Server runtime state; public portion
pub struct Server {
	/// Configured name of server. This is the same as the one in the config
	/// but developers can (and should) reference this string instead.
	pub name: OwnedServerName,

	/// Server-wide configuration instance
	pub config: config::Manager,

	/// Where the configuration came from, replayed on reload.
	pub config_sources: config::Sources,

	/// Timestamp server was started; used for uptime.
	pub started: SystemTime,

	/// Reload/shutdown pending indicator; server is shutting down. This is an
	/// observable used on shutdown and should not be modified.
	pub stopping: AtomicBool,

	/// Reload/shutdown desired indicator; when false, shutdown is desired. This
	/// is an observable used on shutdown and modifying is not recommended.
	pub reloading: AtomicBool,

	/// Restart desired; when true, restart it desired after shutdown.
	pub restarting: AtomicBool,

	/// Set when a backup restore is claimed, which is before it runs and is not
	/// undone if it fails, so a database reopened later in the same process
	/// does not restore a second time. Clearing this re-arms a destructive
	/// operation and is never correct; claim it instead.
	pub backup_restored: AtomicBool,

	/// Handle to the runtime
	pub runtime: Option<runtime::Handle>,

	/// Reload/shutdown signal
	pub signal: broadcast::Sender<&'static str>,

	/// Logging subsystem state
	pub log: Logging,

	/// Metrics subsystem state
	pub metrics: Arc<Metrics>,
}

impl Server {
	#[must_use]
	/// Creates shared server lifecycle state.
	///
	/// The initial configuration, source list, logging state, and metrics
	/// become available to all services. A supplied runtime handle enables
	/// task spawning.
	pub fn new(
		config: Config,
		config_sources: config::Sources,
		runtime: Option<&runtime::Handle>,
		log: Logging,
		metrics: Arc<Metrics>,
	) -> Self {
		Self {
			name: config.server_name.clone(),
			config: config::Manager::new(config),
			config_sources,
			started: SystemTime::now(),
			stopping: AtomicBool::new(false),
			reloading: AtomicBool::new(false),
			restarting: AtomicBool::new(false),
			backup_restored: AtomicBool::new(false),
			runtime: runtime.cloned(),
			signal: broadcast::channel::<&'static str>(1).0,
			log,
			metrics,
		}
	}

	/// Requests a dynamic module reload.
	///
	/// The request marks the server as reloading and stopping before
	/// broadcasting `SIGINT`. Concurrent reload or shutdown requests are
	/// rejected.
	pub fn reload(&self) -> Result {
		if cfg!(any(not(tuwunel_mods), not(feature = "tuwunel_mods"))) {
			return Err!("Reloading not enabled");
		}

		if self.reloading.swap(true, Ordering::AcqRel) {
			return Err!("Reloading already in progress");
		}

		if self.stopping.swap(true, Ordering::AcqRel) {
			return Err!("Shutdown already in progress");
		}

		self.signal("SIGINT").inspect_err(|_| {
			self.stopping.store(false, Ordering::Release);
			self.reloading.store(false, Ordering::Release);
		})
	}

	/// Requests a process restart through the normal shutdown path.
	///
	/// The restarting flag is claimed once before shutdown begins. A rejected
	/// shutdown clears that flag so a later request may retry.
	pub fn restart(&self) -> Result {
		if self.restarting.swap(true, Ordering::AcqRel) {
			return Err!("Restart already in progress");
		}

		self.shutdown().inspect_err(|_| {
			self.restarting.store(false, Ordering::Release);
		})
	}

	/// Requests an orderly server shutdown.
	///
	/// The stopping flag is claimed once before broadcasting `SIGTERM`. A
	/// second request is rejected while shutdown remains in progress.
	pub fn shutdown(&self) -> Result {
		if self.stopping.swap(true, Ordering::AcqRel) {
			return Err!("Shutdown already in progress");
		}

		self.signal("SIGTERM").inspect_err(|_| {
			self.stopping.store(false, Ordering::Release);
		})
	}

	/// Claims the one-shot backup restore, reporting whether this caller is the
	/// one to perform it.
	#[inline]
	pub fn claim_backup_restore(&self) -> bool {
		!self.backup_restored.swap(true, Ordering::AcqRel)
	}

	/// Broadcasts a process-signal name to lifecycle subscribers.
	///
	/// Delivery is best effort because subscribers may not yet be listening.
	/// The method therefore succeeds even when the channel has no receivers.
	pub fn signal(&self, sig: &'static str) -> Result {
		self.signal.send(sig).ok();
		Ok(())
	}

	#[inline]
	/// Waits until the server enters its stopping state.
	///
	/// Lifecycle notifications wake the loop so it can recheck the shared
	/// state. Calling it after shutdown has begun returns immediately.
	pub async fn until_shutdown(self: &Arc<Self>) {
		let mut signal = self.signal.subscribe();
		while self.is_running() {
			signal.recv().await.ok();
		}
	}

	#[inline]
	/// Returns the runtime handle supplied during server construction.
	///
	/// Services use this handle to spawn work on the embedding runtime. The
	/// handle is borrowed for the lifetime of the server.
	///
	/// # Panics
	///
	/// Panics when the server was constructed without a runtime handle.
	pub fn runtime(&self) -> &runtime::Handle {
		self.runtime
			.as_ref()
			.expect("runtime handle available in Server")
	}

	#[inline]
	/// Rejects new work after shutdown begins.
	///
	/// A running server returns success. A stopping server returns an
	/// interrupted I/O error wrapped in the shared error type.
	pub fn check_running(&self) -> Result {
		use std::{io, io::ErrorKind::Interrupted};

		self.is_running()
			.then_some(())
			.ok_or_else(|| io::Error::new(Interrupted, "Server shutting down"))
			.map_err(Into::into)
	}

	#[inline]
	/// Reports whether the server still accepts work.
	///
	/// Running is the inverse of the stopping state. Reload and restart
	/// requests also transition the server through stopping.
	pub fn is_running(&self) -> bool { !self.is_stopping() }

	#[inline]
	/// Reports whether shutdown has begun.
	///
	/// The flag is set by shutdown and reload transitions. Reads are relaxed
	/// because callers use it as a lifecycle observation rather than a
	/// synchronization edge.
	pub fn is_stopping(&self) -> bool { self.stopping.load(Ordering::Relaxed) }

	#[inline]
	/// Reports whether a dynamic module reload is in progress.
	///
	/// Reload claims the flag before checking the stopping state.
	/// Signal-delivery failure clears it, while rejection by an existing
	/// shutdown can leave it set.
	pub fn is_reloading(&self) -> bool { self.reloading.load(Ordering::Relaxed) }

	#[inline]
	/// Reports whether a process restart is in progress.
	///
	/// Restart claims the flag before requesting shutdown. Failed shutdown
	/// initiation clears it for a later attempt.
	pub fn is_restarting(&self) -> bool { self.restarting.load(Ordering::Relaxed) }

	#[inline]
	/// Reports whether a name matches the configured local server name.
	///
	/// The comparison uses the active configuration snapshot. It performs an
	/// exact, case-sensitive string comparison.
	pub fn is_ours(&self, name: &str) -> bool { name == self.config.server_name }
}
