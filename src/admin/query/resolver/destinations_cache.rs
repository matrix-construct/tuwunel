use futures::{StreamExt, TryStreamExt};
use ruma::OwnedServerName;
use tuwunel_core::{
	Result,
	utils::{stream::ReadyExt, time::format as format_time},
};
use tuwunel_service::resolver::cache::CachedDest;

use crate::admin_command;

#[admin_command]
pub(super) async fn destinations_cache(&self, server_name: Option<OwnedServerName>) -> Result {
	writeln!(self, "| Server Name | Destination | Hostname | Expires |").await?;
	writeln!(self, "| ----------- | ----------- | -------- | ------- |").await?;

	self.services
		.resolver
		.cache
		.destinations()
		.ready_filter(|(name, _)| {
			server_name
				.as_deref()
				.is_none_or(|wanted| *name == wanted)
		})
		.map(|(name, CachedDest { dest, host, expire, .. })| {
			let expire = format_time(expire, "%+");

			Ok(format!("| {name} | {dest} | {host} | {expire} |\n"))
		})
		.try_for_each(async |row: String| self.write_str(&row).await)
		.await
}
