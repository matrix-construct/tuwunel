mod notifications;
mod pushers;
mod pushers_set;
mod pushrules;
mod pushrules_global;
mod pushrules_rule;
mod pushrules_rule_actions;
mod pushrules_rule_enabled;

use ruma::{
	UserId,
	events::{GlobalAccountDataEventType, push_rules::PushRulesEvent},
	push::{PredefinedContentRuleId, PredefinedOverrideRuleId},
};
use tuwunel_core::{Result, err};
use tuwunel_service::Services;

pub(crate) use self::{
	notifications::get_notifications_route,
	pushers::get_pushers_route,
	pushers_set::set_pushers_route,
	pushrules::get_pushrules_all_route,
	pushrules_global::get_pushrules_global_route,
	pushrules_rule::{delete_pushrule_route, get_pushrule_route, set_pushrule_route},
	pushrules_rule_actions::{get_pushrule_actions_route, set_pushrule_actions_route},
	pushrules_rule_enabled::{get_pushrule_enabled_route, set_pushrule_enabled_route},
};

async fn load_push_rules(services: &Services, sender_user: &UserId) -> Result<PushRulesEvent> {
	services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await
		.map_err(|_| err!(Request(NotFound("PushRules event not found."))))
}

async fn save_push_rules(
	services: &Services,
	sender_user: &UserId,
	event: &PushRulesEvent,
) -> Result {
	let ty = GlobalAccountDataEventType::PushRules;

	services
		.account_data
		.update(None, sender_user, ty.to_string().into(), &serde_json::to_value(event)?)
		.await
}

// The deprecated mention push rules are hidden from clients as per MSC4210.
#[expect(deprecated)]
fn is_deprecated_mention_rule(rule_id: &str) -> bool {
	rule_id == PredefinedContentRuleId::ContainsUserName.as_str()
		|| rule_id == PredefinedOverrideRuleId::ContainsDisplayName.as_str()
		|| rule_id == PredefinedOverrideRuleId::RoomNotif.as_str()
}
