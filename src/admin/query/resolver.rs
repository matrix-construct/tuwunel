use clap::Subcommand;
use futures::StreamExt;
use ruma::OwnedServerName;
use tuwunel_core::{Result, utils::time};

use crate::{admin_command, admin_command_dispatch};

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
/// Resolver service and caches
pub(crate) enum ResolverCommand {
	/// Query the destinations cache
	DestinationsCache {
		server_name: Option<OwnedServerName>,
	},

	/// Query the overrides cache
	OverridesCache {
		name: Option<String>,
	},
}

#[admin_command]
async fn destinations_cache(&self, server_name: Option<OwnedServerName>) -> Result {
	use tuwunel_service::resolver::cache::CachedDest;

	writeln!(self, "| Server Name | Destination | Hostname | Expires |").await?;
	writeln!(self, "| ----------- | ----------- | -------- | ------- |").await?;

	let mut destinations = self
		.services
		.resolver
		.cache
		.destinations()
		.boxed();

	while let Some((name, CachedDest { dest, host, expire })) = destinations.next().await {
		if let Some(server_name) = server_name.as_ref()
			&& name != server_name
		{
			continue;
		}

		let expire = time::format(expire, "%+");
		self.write_str(&format!("| {name} | {dest} | {host} | {expire} |\n"))
			.await?;
	}

	Ok(())
}

#[admin_command]
async fn overrides_cache(&self, server_name: Option<String>) -> Result {
	use tuwunel_service::resolver::cache::CachedOverride;

	writeln!(self, "| Server Name | IP  | Port | Expires | Overriding |").await?;
	writeln!(self, "| ----------- | --- | ----:| ------- | ---------- |").await?;

	let mut overrides = self.services.resolver.cache.overrides().boxed();

	while let Some((name, CachedOverride { ips, port, expire, overriding })) =
		overrides.next().await
	{
		if let Some(server_name) = server_name.as_ref()
			&& name != server_name
		{
			continue;
		}

		let expire = time::format(expire, "%+");
		self.write_str(&format!("| {name} | {ips:?} | {port} | {expire} | {overriding:?} |\n"))
			.await?;
	}

	Ok(())
}
