use std::{iter, ops::Deref, path::Path, sync::Arc};

use async_trait::async_trait;
use tuwunel_core::{
	Result, Server,
	config::{Config, check},
	error, implement,
};

pub struct Service {
	server: Arc<Server>,
}

const SIGNAL: &str = "SIGUSR1";

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { server: args.server.clone() }))
	}

	async fn worker(self: Arc<Self>) -> Result {
		while self.server.running() {
			if self.server.signal.subscribe().recv().await == Ok(SIGNAL) {
				if let Err(e) = self.handle_reload() {
					error!("Failed to reload config: {e}");
				}
			}
		}

		Ok(())
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Deref for Service {
	type Target = Arc<Config>;

	#[inline]
	fn deref(&self) -> &Self::Target {
		&self.server.config
	}
}

#[implement(Service)]
fn handle_reload(&self) -> Result {
	if self.server.config.config_reload_signal {
		#[cfg(all(feature = "systemd", target_os = "linux"))]
		sd_notify::notify(true, &[sd_notify::NotifyState::Reloading])
			.expect("failed to notify systemd of reloading state");

		self.reload(iter::empty())?;

		#[cfg(all(feature = "systemd", target_os = "linux"))]
		sd_notify::notify(true, &[sd_notify::NotifyState::Ready])
			.expect("failed to notify systemd of ready state");
	}

	Ok(())
}

#[implement(Service)]
pub fn reload<'a, I>(&self, paths: I) -> Result<Arc<Config>>
where
	I: Iterator<Item = &'a Path>,
{
	let old = self.server.config.clone();
	let new = Config::load(paths).and_then(|raw| Config::new(&raw))?;

	check::reload(&old, &new)?;
	self.server.config.update(new)
}
