use axum::extract::State;
use futures::StreamExt;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, MilliSecondsSinceUnixEpoch,
	api::client::{
		error::ErrorKind,
		push::{
			delete_pushrule, get_notifications, get_pushers, get_pushrule, get_pushrule_actions,
			get_pushrule_enabled, get_pushrules_all, get_pushrules_global_scope, set_pusher,
			set_pushrule, set_pushrule_actions, set_pushrule_enabled,
		},
	},
	events::{
		GlobalAccountDataEventType,
		push_rules::{PushRulesEvent, PushRulesEventContent},
	},
	push::{
		Action, InsertPushRuleError, PredefinedContentRuleId, PredefinedOverrideRuleId,
		RemovePushRuleError, Ruleset,
	},
};
use tuwunel_core::{
	Err, Error, Result, at, err,
	matrix::{Event, PduId},
	utils::{
		stream::{ReadyExt, WidebandExt},
		string::to_small_string,
	},
};
use tuwunel_service::Services;

use crate::Ruma;

/// # `GET /_matrix/client/r0/notifications/`
///
/// Paginate through the list of events the user has been, or would have been
/// notified about.
pub(crate) async fn get_notifications_route(
	State(services): State<crate::State>,
	body: Ruma<get_notifications::v3::Request>,
) -> Result<get_notifications::v3::Response> {
	use get_notifications::v3::Notification;

	let sender_user = body.sender_user();

	let from = body
		.body
		.from
		.as_deref()
		.map(str::parse)
		.transpose()
		.map_err(|e| err!(Request(InvalidParam("Invalid `from' parameter: {e}"))))?;

	let limit: usize = body
		.body
		.limit
		.map(TryInto::try_into)
		.transpose()?
		.unwrap_or(50)
		.clamp(1, 100);

	let only_highlight = body
		.body
		.only
		.as_deref()
		.is_some_and(|only| only.contains("highlight"));

	let mut next_token: Option<u64> = None;
	let notifications = services
		.pusher
		.get_notifications(sender_user, from)
		.ready_filter(|(_, notify)| {
			if only_highlight && !notify.actions.iter().any(Action::is_highlight) {
				return false;
			}

			true
		})
		.wide_filter_map(async |(count, notify)| {
			let pdu_id = PduId {
				shortroomid: notify.sroomid,
				count: count.into(),
			};

			let event = services
				.timeline
				.get_pdu_from_id(&pdu_id.into())
				.await
				.ok()
				.filter(|event| !event.is_redacted())?;

			let read = services
				.pusher
				.last_notification_read(sender_user, event.room_id())
				.await
				.is_ok_and(|last_read| last_read.ge(&count));

			let ts = notify
				.ts
				.try_into()
				.map(MilliSecondsSinceUnixEpoch)
				.ok()?;

			let notification = Notification {
				room_id: event.room_id().into(),
				event: event.into_format(),
				ts,
				read,
				profile_tag: notify.tag,
				actions: notify.actions,
			};

			Some((count, notification))
		})
		.take(limit)
		.inspect(|(count, _)| {
			next_token.replace(*count);
		})
		.map(at!(1))
		.collect::<Vec<_>>()
		.await;

	Ok(get_notifications::v3::Response {
		next_token: next_token.map(to_small_string),
		notifications,
	})
}

/// # `GET /_matrix/client/r0/pushrules/`
///
/// Retrieves the push rules event for this user.
pub(crate) async fn get_pushrules_all_route(
	State(services): State<crate::State>,
	body: Ruma<get_pushrules_all::v3::Request>,
) -> Result<get_pushrules_all::v3::Response> {
	let sender_user = body.sender_user();

	let Some(content_value) = services
		.account_data
		.get_global::<CanonicalJsonObject>(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.ok()
		.and_then(|event| event.get("content").cloned())
		.filter(CanonicalJsonValue::is_object)
	else {
		// user somehow has non-existent push rule event. recreate it and return server
		// default silently
		return recreate_push_rules_and_return(&services, sender_user).await;
	};

	let account_data_content =
		serde_json::from_value::<PushRulesEventContent>(content_value.into()).map_err(|e| {
			err!(Database(warn!("Invalid push rules account data event in database: {e}")))
		})?;

	let mut global_ruleset = account_data_content.global;

	// remove old deprecated mentions push rules as per MSC4210
	// and update the stored server default push rules
	#[expect(deprecated)]
	{
		use ruma::push::RuleKind::*;
		if global_ruleset
			.get(Override, PredefinedOverrideRuleId::ContainsDisplayName.as_str())
			.is_some()
			|| global_ruleset
				.get(Override, PredefinedOverrideRuleId::RoomNotif.as_str())
				.is_some()
			|| global_ruleset
				.get(Content, PredefinedContentRuleId::ContainsUserName.as_str())
				.is_some()
		{
			global_ruleset
				.remove(Override, PredefinedOverrideRuleId::ContainsDisplayName)
				.ok();
			global_ruleset
				.remove(Override, PredefinedOverrideRuleId::RoomNotif)
				.ok();
			global_ruleset
				.remove(Content, PredefinedContentRuleId::ContainsUserName)
				.ok();

			global_ruleset.update_with_server_default(Ruleset::server_default(sender_user));

			let ty = GlobalAccountDataEventType::PushRules;
			let event = PushRulesEvent {
				content: PushRulesEventContent { global: global_ruleset.clone() },
			};

			services
				.account_data
				.update(None, sender_user, ty.to_string().into(), &serde_json::to_value(event)?)
				.await?;
		}
	};

	Ok(get_pushrules_all::v3::Response { global: global_ruleset })
}

/// # `GET /_matrix/client/r0/pushrules/global/`
///
/// Retrieves the push rules event for this user.
///
/// This appears to be the exact same as `GET /_matrix/client/r0/pushrules/`.
pub(crate) async fn get_pushrules_global_route(
	State(services): State<crate::State>,
	body: Ruma<get_pushrules_global_scope::v3::Request>,
) -> Result<get_pushrules_global_scope::v3::Response> {
	let sender_user = body.sender_user();

	let Some(content_value) = services
		.account_data
		.get_global::<CanonicalJsonObject>(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.ok()
		.and_then(|event| event.get("content").cloned())
		.filter(CanonicalJsonValue::is_object)
	else {
		// user somehow has non-existent push rule event. recreate it and return server
		// default silently

		let ty = GlobalAccountDataEventType::PushRules;
		let event = PushRulesEvent {
			content: PushRulesEventContent {
				global: Ruleset::server_default(sender_user),
			},
		};

		services
			.account_data
			.update(None, sender_user, ty.to_string().into(), &serde_json::to_value(event)?)
			.await?;

		return Ok(get_pushrules_global_scope::v3::Response {
			global: Ruleset::server_default(sender_user),
		});
	};

	let account_data_content =
		serde_json::from_value::<PushRulesEventContent>(content_value.into()).map_err(|e| {
			err!(Database(warn!("Invalid push rules account data event in database: {e}")))
		})?;

	let mut global_ruleset = account_data_content.global;

	// remove old deprecated mentions push rules as per MSC4210
	// and update the stored server default push rules
	#[expect(deprecated)]
	{
		use ruma::push::RuleKind::*;
		if global_ruleset
			.get(Override, PredefinedOverrideRuleId::ContainsDisplayName.as_str())
			.is_some()
			|| global_ruleset
				.get(Override, PredefinedOverrideRuleId::RoomNotif.as_str())
				.is_some()
			|| global_ruleset
				.get(Content, PredefinedContentRuleId::ContainsUserName.as_str())
				.is_some()
		{
			global_ruleset
				.remove(Override, PredefinedOverrideRuleId::ContainsDisplayName)
				.ok();
			global_ruleset
				.remove(Override, PredefinedOverrideRuleId::RoomNotif)
				.ok();
			global_ruleset
				.remove(Content, PredefinedContentRuleId::ContainsUserName)
				.ok();

			global_ruleset.update_with_server_default(Ruleset::server_default(sender_user));

			services
				.account_data
				.update(
					None,
					sender_user,
					GlobalAccountDataEventType::PushRules
						.to_string()
						.into(),
					&serde_json::to_value(PushRulesEvent {
						content: PushRulesEventContent { global: global_ruleset.clone() },
					})
					.expect("to json always works"),
				)
				.await?;
		}
	};

	Ok(get_pushrules_global_scope::v3::Response { global: global_ruleset })
}

/// # `GET /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}`
///
/// Retrieves a single specified push rule for this user.
pub(crate) async fn get_pushrule_route(
	State(services): State<crate::State>,
	body: Ruma<get_pushrule::v3::Request>,
) -> Result<get_pushrule::v3::Response> {
	let sender_user = body
		.sender_user
		.as_ref()
		.expect("user is authenticated");

	// remove old deprecated mentions push rules as per MSC4210
	#[expect(deprecated)]
	if body.rule_id.as_str() == PredefinedContentRuleId::ContainsUserName.as_str()
		|| body.rule_id.as_str() == PredefinedOverrideRuleId::ContainsDisplayName.as_str()
		|| body.rule_id.as_str() == PredefinedOverrideRuleId::RoomNotif.as_str()
	{
		return Err!(Request(NotFound("Push rule not found.")));
	}

	let event: PushRulesEvent = services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.map_err(|_| err!(Request(NotFound("PushRules event not found."))))?;

	let rule = event
		.content
		.global
		.get(body.kind.clone(), &body.rule_id)
		.map(Into::into);

	if let Some(rule) = rule {
		Ok(get_pushrule::v3::Response { rule })
	} else {
		Err!(Request(NotFound("Push rule not found.")))
	}
}

/// # `PUT /_matrix/client/r0/pushrules/global/{kind}/{ruleId}`
///
/// Creates a single specified push rule for this user.
pub(crate) async fn set_pushrule_route(
	State(services): State<crate::State>,
	body: Ruma<set_pushrule::v3::Request>,
) -> Result<set_pushrule::v3::Response> {
	let sender_user = body.sender_user();
	let body = &body.body;
	let mut account_data: PushRulesEvent = services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.map_err(|_| err!(Request(NotFound("PushRules event not found."))))?;

	if let Err(error) = account_data.content.global.insert(
		body.rule.clone(),
		body.after.as_deref(),
		body.before.as_deref(),
	) {
		let err = match error {
			| InsertPushRuleError::ServerDefaultRuleId => Error::BadRequest(
				ErrorKind::InvalidParam,
				"Rule IDs starting with a dot are reserved for server-default rules.",
			),
			| InsertPushRuleError::InvalidRuleId => Error::BadRequest(
				ErrorKind::InvalidParam,
				"Rule ID containing invalid characters.",
			),
			| InsertPushRuleError::RelativeToServerDefaultRule => Error::BadRequest(
				ErrorKind::InvalidParam,
				"Can't place a push rule relatively to a server-default rule.",
			),
			| InsertPushRuleError::UnknownRuleId => Error::BadRequest(
				ErrorKind::NotFound,
				"The before or after rule could not be found.",
			),
			| InsertPushRuleError::BeforeHigherThanAfter => Error::BadRequest(
				ErrorKind::InvalidParam,
				"The before rule has a higher priority than the after rule.",
			),
			| _ => Error::BadRequest(ErrorKind::InvalidParam, "Invalid data."),
		};

		return Err(err);
	}

	let ty = GlobalAccountDataEventType::PushRules;
	services
		.account_data
		.update(None, sender_user, ty.to_string().into(), &serde_json::to_value(account_data)?)
		.await?;

	Ok(set_pushrule::v3::Response {})
}

/// # `GET /_matrix/client/r0/pushrules/global/{kind}/{ruleId}/actions`
///
/// Gets the actions of a single specified push rule for this user.
pub(crate) async fn get_pushrule_actions_route(
	State(services): State<crate::State>,
	body: Ruma<get_pushrule_actions::v3::Request>,
) -> Result<get_pushrule_actions::v3::Response> {
	let sender_user = body.sender_user();

	// remove old deprecated mentions push rules as per MSC4210
	#[expect(deprecated)]
	if body.rule_id.as_str() == PredefinedContentRuleId::ContainsUserName.as_str()
		|| body.rule_id.as_str() == PredefinedOverrideRuleId::ContainsDisplayName.as_str()
		|| body.rule_id.as_str() == PredefinedOverrideRuleId::RoomNotif.as_str()
	{
		return Err!(Request(NotFound("Push rule not found.")));
	}

	let event: PushRulesEvent = services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.map_err(|_| err!(Request(NotFound("PushRules event not found."))))?;

	let actions = event
		.content
		.global
		.get(body.kind.clone(), &body.rule_id)
		.map(|rule| rule.actions().to_owned())
		.ok_or_else(|| err!(Request(NotFound("Push rule not found."))))?;

	Ok(get_pushrule_actions::v3::Response { actions })
}

/// # `PUT /_matrix/client/r0/pushrules/global/{kind}/{ruleId}/actions`
///
/// Sets the actions of a single specified push rule for this user.
pub(crate) async fn set_pushrule_actions_route(
	State(services): State<crate::State>,
	body: Ruma<set_pushrule_actions::v3::Request>,
) -> Result<set_pushrule_actions::v3::Response> {
	let sender_user = body.sender_user();

	let mut account_data: PushRulesEvent = services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.map_err(|_| err!(Request(NotFound("PushRules event not found."))))?;

	if account_data
		.content
		.global
		.set_actions(body.kind.clone(), &body.rule_id, body.actions.clone().into())
		.is_err()
	{
		return Err!(Request(NotFound("Push rule not found.")));
	}

	let ty = GlobalAccountDataEventType::PushRules;
	services
		.account_data
		.update(None, sender_user, ty.to_string().into(), &serde_json::to_value(account_data)?)
		.await?;

	Ok(set_pushrule_actions::v3::Response {})
}

/// # `GET /_matrix/client/r0/pushrules/global/{kind}/{ruleId}/enabled`
///
/// Gets the enabled status of a single specified push rule for this user.
pub(crate) async fn get_pushrule_enabled_route(
	State(services): State<crate::State>,
	body: Ruma<get_pushrule_enabled::v3::Request>,
) -> Result<get_pushrule_enabled::v3::Response> {
	let sender_user = body.sender_user();

	// remove old deprecated mentions push rules as per MSC4210
	#[expect(deprecated)]
	if body.rule_id.as_str() == PredefinedContentRuleId::ContainsUserName.as_str()
		|| body.rule_id.as_str() == PredefinedOverrideRuleId::ContainsDisplayName.as_str()
		|| body.rule_id.as_str() == PredefinedOverrideRuleId::RoomNotif.as_str()
	{
		return Ok(get_pushrule_enabled::v3::Response { enabled: false });
	}

	let event: PushRulesEvent = services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.map_err(|_| err!(Request(NotFound("PushRules event not found."))))?;

	let enabled = event
		.content
		.global
		.get(body.kind.clone(), &body.rule_id)
		.map(ruma::push::AnyPushRuleRef::enabled)
		.ok_or_else(|| err!(Request(NotFound("Push rule not found."))))?;

	Ok(get_pushrule_enabled::v3::Response { enabled })
}

/// # `PUT /_matrix/client/r0/pushrules/global/{kind}/{ruleId}/enabled`
///
/// Sets the enabled status of a single specified push rule for this user.
pub(crate) async fn set_pushrule_enabled_route(
	State(services): State<crate::State>,
	body: Ruma<set_pushrule_enabled::v3::Request>,
) -> Result<set_pushrule_enabled::v3::Response> {
	let sender_user = body.sender_user();

	let mut account_data: PushRulesEvent = services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.map_err(|_| err!(Request(NotFound("PushRules event not found."))))?;

	if account_data
		.content
		.global
		.set_enabled(body.kind.clone(), &body.rule_id, body.enabled)
		.is_err()
	{
		return Err!(Request(NotFound("Push rule not found.")));
	}

	let ty = GlobalAccountDataEventType::PushRules;
	services
		.account_data
		.update(None, sender_user, ty.to_string().into(), &serde_json::to_value(account_data)?)
		.await?;

	Ok(set_pushrule_enabled::v3::Response {})
}

/// # `DELETE /_matrix/client/r0/pushrules/global/{kind}/{ruleId}`
///
/// Deletes a single specified push rule for this user.
pub(crate) async fn delete_pushrule_route(
	State(services): State<crate::State>,
	body: Ruma<delete_pushrule::v3::Request>,
) -> Result<delete_pushrule::v3::Response> {
	let sender_user = body.sender_user();

	let mut account_data: PushRulesEvent = services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.map_err(|_| err!(Request(NotFound("PushRules event not found."))))?;

	if let Err(error) = account_data
		.content
		.global
		.remove(body.kind.clone(), &body.rule_id)
	{
		let err = match error {
			| RemovePushRuleError::ServerDefault => Error::BadRequest(
				ErrorKind::InvalidParam,
				"Cannot delete a server-default pushrule.",
			),
			| RemovePushRuleError::NotFound =>
				Error::BadRequest(ErrorKind::NotFound, "Push rule not found."),
			| _ => Error::BadRequest(ErrorKind::InvalidParam, "Invalid data."),
		};

		return Err(err);
	}

	let ty = GlobalAccountDataEventType::PushRules;
	services
		.account_data
		.update(None, sender_user, ty.to_string().into(), &serde_json::to_value(account_data)?)
		.await?;

	Ok(delete_pushrule::v3::Response {})
}

/// # `GET /_matrix/client/r0/pushers`
///
/// Gets all currently active pushers for the sender user.
pub(crate) async fn get_pushers_route(
	State(services): State<crate::State>,
	body: Ruma<get_pushers::v3::Request>,
) -> Result<get_pushers::v3::Response> {
	let sender_user = body.sender_user();

	Ok(get_pushers::v3::Response {
		pushers: services.pusher.get_pushers(sender_user).await,
	})
}

/// # `POST /_matrix/client/r0/pushers/set`
///
/// Adds a pusher for the sender user.
///
/// - TODO: Handle `append`
pub(crate) async fn set_pushers_route(
	State(services): State<crate::State>,
	body: Ruma<set_pusher::v3::Request>,
) -> Result<set_pusher::v3::Response> {
	let sender_user = body.sender_user();

	services
		.pusher
		.set_pusher(sender_user, body.sender_device()?, &body.action)
		.await?;

	Ok(set_pusher::v3::Response::new())
}

/// user somehow has bad push rules, these must always exist per spec.
/// so recreate it and return server default silently
async fn recreate_push_rules_and_return(
	services: &Services,
	sender_user: &ruma::UserId,
) -> Result<get_pushrules_all::v3::Response> {
	let ty = GlobalAccountDataEventType::PushRules;
	let event = PushRulesEvent {
		content: PushRulesEventContent {
			global: Ruleset::server_default(sender_user),
		},
	};

	services
		.account_data
		.update(None, sender_user, ty.to_string().into(), &serde_json::to_value(event)?)
		.await?;

	Ok(get_pushrules_all::v3::Response {
		global: Ruleset::server_default(sender_user),
	})
}
