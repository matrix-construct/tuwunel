use futures::StreamExt;
use tuwunel_core::{Result, utils::time};
use tuwunel_service::resolver::cache::CachedOverride;

use crate::admin_command;

#[admin_command]
pub(super) async fn overrides_cache(&self, server_name: Option<String>) -> Result {
	writeln!(self, "| Server Name | IP  | Port | Expires | Overriding |").await?;
	writeln!(self, "| ----------- | --- | ----:| ------- | ---------- |").await?;

	let mut overrides = self.services.resolver.cache.overrides().boxed(); // Unpin for next

	while let Some((name, CachedOverride { ips, port, expire, overriding })) =
		overrides.next().await
	{
		if let Some(server_name) = server_name.as_ref()
			&& name != server_name
		{
			continue;
		}

		let expire = time::format(expire, "%+");
		write!(self, "| {name} | {ips:?} | {port} | {expire} | {overriding:?} |\n").await?;
	}

	Ok(())
}
