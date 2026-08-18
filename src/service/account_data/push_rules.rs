use ruma::{
	RoomId, UserId,
	events::{GlobalAccountDataEventType, push_rules::PushRulesEvent},
	push::{AnyPushRuleRef, NewPushRule, NewSimplePushRule, RuleKind},
};
use tuwunel_core::{Result, debug_warn, err, implement, utils::json::serialized_len};

use super::{MAX_RULE_BYTES, admits_rule};

/// Copies a room's push rule to a successor room.
///
/// The source rule's enabled state travels with it, so a rule the user had
/// switched off does not come back on in the successor. A destination rule that
/// already exists keeps its own enabled state and takes only the actions.
#[implement(super::Service)]
pub async fn copy_room_push_rule(
	&self,
	user_id: &UserId,
	from_room: &RoomId,
	to_room: &RoomId,
) -> Result {
	let Ok(mut account_data): Result<PushRulesEvent> = self
		.get_global(user_id, GlobalAccountDataEventType::PushRules)
		.await
	else {
		return Ok(());
	};

	let ruleset = &mut account_data.content.global;

	let Some(AnyPushRuleRef::Room(rule)) = ruleset.get(RuleKind::Room, from_room) else {
		return Ok(());
	};

	let actions = rule.actions.clone();
	let enabled = rule.enabled;

	if !admits_rule(ruleset, RuleKind::Room, to_room.as_str())
		|| serialized_len(&actions)? > MAX_RULE_BYTES
	{
		debug_warn!(%user_id, %to_room, "Ruleset has no room for the copied push rule");

		return Ok(());
	}

	let rule = NewPushRule::Room(NewSimplePushRule::new(to_room.to_owned(), actions));
	let is_new = ruleset.get(RuleKind::Room, to_room).is_none();

	ruleset
		.insert(rule, None, None)
		.map_err(|e| err!(Request(InvalidParam("Failed to copy room push rule: {e}"))))?;

	// Insert already inherits a pre-existing destination rule's enabled state.
	if is_new {
		ruleset
			.set_enabled(RuleKind::Room, to_room, enabled)
			.map_err(|e| err!(Request(InvalidParam("Failed to copy push rule state: {e}"))))?;
	}

	let ty = GlobalAccountDataEventType::PushRules;
	let data = serde_json::to_value(account_data)?;

	self.update(None, user_id, ty.to_string().into(), &data)
		.await
}
