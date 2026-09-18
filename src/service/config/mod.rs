//! Reloadable homeserver configuration.
//!
//! The service exposes the active configuration and replaces it after validation.
//! Reloads replay the startup sources so command-line paths and overrides remain in force.

#[cfg(all(feature = "systemd", target_os = "linux"))]
use std::borrow::Cow;
use std::{iter::empty, ops::Deref, path::Path, sync::Arc};

use async_trait::async_trait;
#[cfg(all(feature = "systemd", target_os = "linux"))]
use sd_notify::{NotifyState, notify};
#[cfg(all(feature = "systemd", target_os = "linux"))]
use tuwunel_core::itertools::Itertools;
use tuwunel_core::{
	Result, Server,
	config::{Config, check},
	error, implement,
};

/// Provides access to the active configuration and its reload path.
///
/// Values are read through the shared server configuration handle. Reloads publish a
/// replacement only after the new configuration passes the reload checks.
pub struct Service {
	server: Arc<Server>,
}

const SIGNAL: &str = "SIGUSR1";

/// Cap on the status reported to the service manager, which displays one line.
#[cfg(all(feature = "systemd", target_os = "linux"))]
const STATUS_MAX: usize = 192;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { server: args.server.clone() }))
	}

	async fn worker(self: Arc<Self>) -> Result {
		let mut signaled = self.server.signal.subscribe();
		while self.server.is_running() {
			tokio::select! {
				() = self.server.until_shutdown() => break,
				signal = signaled.recv() => if signal !=  Ok(SIGNAL) { continue; },
			}

			if let Err(e) = self.handle_reload() {
				error!("Failed to reload config: {e}");
			}
		}

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Deref for Service {
	type Target = Arc<Config>;

	#[inline]
	fn deref(&self) -> &Self::Target { &self.server.config }
}

#[implement(Service)]
fn handle_reload(&self) -> Result {
	// The handshake completes even when reloading is switched off, since the
	// service manager is already waiting on it by the time the signal arrives.
	#[cfg(all(feature = "systemd", target_os = "linux"))]
	NotifyState::monotonic_usec_now()
		.and_then(|monotonic| notify(&[NotifyState::Reloading, monotonic]))
		.inspect_err(|e| error!(%e, "failed to notify systemd of reloading state"))
		.ok();

	let reloaded = self
		.server
		.config
		.config_reload_signal
		.then(|| self.reload(empty()))
		.transpose();

	// Ready even on failure, since the old config stays in service; the outcome
	// travels in the status string instead.
	#[cfg(all(feature = "systemd", target_os = "linux"))]
	{
		let status: Cow<'_, str> = match &reloaded {
			| Ok(Some(_)) => "Configuration reloaded".into(),
			| Ok(None) => "Configuration reloading is disabled".into(),
			| Err(e) => format!("Configuration rejected: {e}").into(),
		};

		notify(&[NotifyState::Ready, NotifyState::Status(&one_line(&status))])
			.inspect_err(|e| error!(%e, "failed to notify systemd of ready state"))
			.ok();
	};

	reloaded?;

	Ok(())
}

/// The notify protocol delimits assignments by newline and does no escaping, so
/// a status carrying one would be read as further assignments.
#[cfg(all(feature = "systemd", target_os = "linux"))]
fn one_line(status: &str) -> String {
	status
		.split_whitespace()
		.join(" ")
		.chars()
		.take(STATUS_MAX)
		.collect()
}

/// Loads, validates, and publishes a replacement configuration.
///
/// The stored startup sources are replayed together with `paths`. The active configuration
/// remains unchanged when loading or reload validation fails.
#[implement(Service)]
pub fn reload<'a, I>(&'a self, paths: I) -> Result<Arc<Config>>
where
	I: Iterator<Item = &'a Path>,
{
	let old = self.server.config.clone();

	// Replay the startup command line so -c paths and -O overrides survive.
	let new = self
		.server
		.config_sources
		.load(paths)
		.and_then(|raw| Config::new(&raw))?;

	check::reload(&old, &new)?;
	self.server.config.update(new)
}
