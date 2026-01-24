use futures::StreamExt;
use tuwunel_core::{Err, Result, checked, err};

use crate::admin_command;

#[admin_command]
pub(super) async fn appservice_register(&self) -> Result {
	let body = &self.body;
	let body_len = self.body.len();
	if body_len < 2
		|| !body[0].trim().starts_with("```")
		|| body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.");
	}

	let range = 1..checked!(body_len - 1)?;
	let appservice_config_body = body[range].join("\n");
	let parsed_config = serde_yaml::from_str(&appservice_config_body);

	let registration =
		parsed_config.map_err(|e| err!("Could not parse appservice config as YAML: {e}"))?;

	self.services
		.appservice
		.register_appservice(&registration, &appservice_config_body)
		.await
		.map_err(|e| err!("Failed to register appservice: {e}"))?;

	self.write_str(&format!("Appservice registered with ID: {}", registration.id))
		.await
}

#[admin_command]
pub(super) async fn appservice_unregister(&self, appservice_identifier: String) -> Result {
	self.services
		.appservice
		.unregister_appservice(&appservice_identifier)
		.await
		.map_err(|e| err!("Failed to unregister appservice: {e}"))?;

	self.write_str("Appservice unregistered.").await
}

#[admin_command]
pub(super) async fn appservice_show_config(&self, appservice_identifier: String) -> Result {
	let config = self
		.services
		.appservice
		.get_registration(&appservice_identifier)
		.await
		.ok_or(err!("Appservice does not exist."))?;

	let config_str = serde_yaml::to_string(&config)?;

	self.write_str(&format!("Config for {appservice_identifier}:\n\n```yaml\n{config_str}\n```"))
		.await
}

#[admin_command]
pub(super) async fn appservice_list(&self) -> Result {
	let appservices: Vec<_> = self
		.services
		.appservice
		.iter_ids()
		.collect()
		.await;

	let len = appservices.len();
	let list = appservices.join(", ");
	self.write_str(&format!("Appservices ({len}): {list}"))
		.await
}
