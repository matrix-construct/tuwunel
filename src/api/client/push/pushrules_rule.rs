use axum::extract::State;
use ruma::{
	api::client::push::{
		delete_pushrule,
		get_pushrule::{self, v3::Response},
		set_pushrule,
	},
	push::{InsertPushRuleError, RemovePushRuleError},
};
use tuwunel_core::{Err, Result};

use crate::Ruma;

/// # `GET /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}`
///
/// Retrieves a single specified push rule for this user.
pub(crate) async fn get_pushrule_route(
	State(services): State<crate::State>,
	body: Ruma<get_pushrule::v3::Request>,
) -> Result<Response> {
	let sender_user = body
		.sender_user
		.as_ref()
		.expect("user is authenticated");

	if super::is_deprecated_mention_rule(body.rule_id.as_str()) {
		return Err!(Request(NotFound("Push rule not found.")));
	}

	let event = super::load_push_rules(&services, sender_user).await?;

	event
		.content
		.global
		.get(body.kind.clone(), &body.rule_id)
		.map(Into::into)
		.map_or_else(
			|| Err!(Request(NotFound("Push rule not found."))),
			|rule| Ok(Response { rule }),
		)
}

/// # `PUT /_matrix/client/r0/pushrules/global/{kind}/{ruleId}`
///
/// Creates a single specified push rule for this user.
pub(crate) async fn set_pushrule_route(
	State(services): State<crate::State>,
	body: Ruma<set_pushrule::v3::Request>,
) -> Result<set_pushrule::v3::Response> {
	let sender_user = body.sender_user();
	let mut account_data = super::load_push_rules(&services, sender_user).await?;

	if let Err(error) = account_data.content.global.insert(
		body.rule.clone(),
		body.after.as_deref(),
		body.before.as_deref(),
	) {
		use InsertPushRuleError::*;

		return match error {
			| ServerDefaultRuleId => Err!(Request(InvalidParam(
				"Rule IDs starting with a dot are reserved for server-default rules."
			))),
			| RelativeToServerDefaultRule => Err!(Request(InvalidParam(
				"Can't place a push rule relatively to a server-default rule."
			))),
			| BeforeHigherThanAfter => Err!(Request(InvalidParam(
				"The before rule has a higher priority than the after rule."
			))),
			| InvalidRuleId =>
				Err!(Request(InvalidParam("Rule ID containing invalid characters."))),

			| UnknownRuleId =>
				Err!(Request(NotFound("The before or after rule could not be found."))),

			| _ => Err!(Request(InvalidParam("Invalid data."))),
		};
	}

	super::save_push_rules(&services, sender_user, &account_data).await?;

	Ok(set_pushrule::v3::Response {})
}

/// # `DELETE /_matrix/client/r0/pushrules/global/{kind}/{ruleId}`
///
/// Deletes a single specified push rule for this user.
pub(crate) async fn delete_pushrule_route(
	State(services): State<crate::State>,
	body: Ruma<delete_pushrule::v3::Request>,
) -> Result<delete_pushrule::v3::Response> {
	let sender_user = body.sender_user();
	let mut account_data = super::load_push_rules(&services, sender_user).await?;

	if let Err(error) = account_data
		.content
		.global
		.remove(body.kind.clone(), &body.rule_id)
	{
		return match error {
			| RemovePushRuleError::ServerDefault =>
				Err!(Request(InvalidParam("Cannot delete a server-default pushrule."))),

			| RemovePushRuleError::NotFound => Err!(Request(NotFound("Push rule not found."))),

			| _ => Err!(Request(InvalidParam("Invalid data."))),
		};
	}

	super::save_push_rules(&services, sender_user, &account_data).await?;

	Ok(delete_pushrule::v3::Response {})
}
