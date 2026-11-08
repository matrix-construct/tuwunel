use tuwunel_core::{
	Result,
	info::cargo::{dependencies, dependencies_names},
	itertools::Itertools,
};

use crate::admin_command;

#[admin_command]
pub(super) async fn list_dependencies(&self, names: bool) -> Result {
	if names {
		let out = dependencies_names().join(" ");

		return self.write_str(&out).await;
	}

	writeln!(self, "| name | version | features |").await?;
	writeln!(self, "| ---- | ------- | -------- |").await?;
	for (name, version, features) in dependencies() {
		let features = features.iter().format(" ");

		writeln!(self, "| {name} | {version} | {features} |").await?;
	}

	Ok(())
}
