use tuwunel_core::{
	Result,
	either::Either::{Left, Right},
	info,
};

use crate::admin_command;

#[admin_command]
pub(super) async fn list_dependencies(&self, names: bool) -> Result {
	if names {
		let out = info::cargo::dependencies_names().join(" ");
		return self.write_str(&out).await;
	}

	let deps = info::cargo::dependencies();
	writeln!(self, "| name | version | features |").await?;
	writeln!(self, "| ---- | ------- | -------- |").await?;
	for (name, dep) in deps {
		let version = dep.try_req().map_or(Right("*"), Left);
		let feats = dep.req_features();
		let feats = if !feats.is_empty() {
			feats.join(" ")
		} else {
			String::new()
		};

		writeln!(self, "| {name} | {version} | {feats} |").await?;
	}

	Ok(())
}
