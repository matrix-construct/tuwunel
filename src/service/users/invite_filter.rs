use futures::{TryFutureExt, future::join4};
use ruma::{
	UserId,
	events::{
		GlobalAccountDataEventType,
		ignored_user_list::{IgnoredUserListEvent, IgnoredUserListEventContent},
		invite_permission_config::{
			InvitePermission, InvitePermissionAction, InvitePermissionConfigEvent,
			InvitePermissionConfigEventContent, UnstableInvitePermissionConfigEvent,
			UnstableInvitePermissionConfigEventContent,
		},
	},
};
use serde::Deserialize;
use tuwunel_core::{implement, utils::future::TryExtExt};

/// A recipient's invite filtering state, evaluable against any sender.
///
/// Holds the ignored-user list alongside every `invite_permission_config`
/// account data slot a client may have written. The slots are read together
/// once and then judged per sender, so a caller facing many invites pays for
/// the account data once.
pub struct InviteFilter {
	ignored: Option<IgnoredUserListEventContent>,
	stable: Option<InvitePermissionConfigEventContent>,
	unstable_blanket: Option<InvitePermissionConfigEventContent>,
	unstable_lists: Option<InvitePermissionConfigEventContent>,
}

/// The account-data slot for MSC4380 configs written before spec adoption.
///
/// Carries the blanket switch alone, which reads into the same content type as
/// a `default_action` of `block`. A block written here outlives the stable
/// slot, since clearing `default_action` there leaves this one standing and no
/// client writes this type any more.
const BLANKET_TYPE: &str = "org.matrix.msc4380.invite_permission_config";

/// The account-data slot for MSC4155 configs written before spec adoption.
///
/// Carries the rule lists, which the spec has not adopted, so this slot is
/// the only place a client can write them today.
const LISTS_TYPE: &str = "org.matrix.msc4155.invite_permission_config";

/// Fetch the invite filtering state a recipient has configured.
///
/// The four account data slots are read concurrently, and a slot that is
/// absent or malformed drops out rather than failing the others.
#[implement(super::Service)]
pub async fn invite_filter(&self, recipient: &UserId) -> InviteFilter {
	let ignored = self
		.services
		.account_data
		.get_global(recipient, GlobalAccountDataEventType::IgnoredUserList)
		.map_ok(|event: IgnoredUserListEvent| event.content)
		.ok();

	let stable = self.stable_config(recipient);

	let unstable_blanket = self
		.services
		.account_data
		.get_global(recipient, BLANKET_TYPE.into())
		.map_ok(|event: UnstableInvitePermissionConfigEvent| event.content.into())
		.ok();

	let unstable_lists = self
		.services
		.account_data
		.get_global(recipient, LISTS_TYPE.into())
		.map_ok(|event: InvitePermissionConfigEvent| event.content)
		.ok();

	let (ignored, stable, unstable_blanket, unstable_lists) =
		join4(ignored, stable, unstable_blanket, unstable_lists).await;

	InviteFilter {
		ignored,
		stable,
		unstable_blanket,
		unstable_lists,
	}
}

/// The stable slot, keeping its blanket switch even when its lists are not.
///
/// MSC4155 invalidates the whole event when a rule list is not an array, but
/// the blanket block is stable spec and outranks the lists, so it is read back
/// on its own rather than lost with them.
#[implement(super::Service)]
async fn stable_config(&self, recipient: &UserId) -> Option<InvitePermissionConfigEventContent> {
	let kind = GlobalAccountDataEventType::InvitePermissionConfig;

	let config = self
		.services
		.account_data
		.get_global(recipient, kind.clone())
		.map_ok(|event: InvitePermissionConfigEvent| event.content)
		.await;

	match config {
		| Ok(content) => Some(content),
		| Err(_) => self
			.services
			.account_data
			.get_global(recipient, kind)
			.map_ok(|event: BlanketEvent| event.content.default_action)
			.await
			.is_ok_and(|action| matches!(action, Some(InvitePermissionAction::Block)))
			.then(|| UnstableInvitePermissionConfigEventContent::new(true).into()),
	}
}

/// The blanket switch alone, for an event MSC4155 considers invalid.
#[derive(Deserialize)]
struct BlanketEvent {
	content: BlanketContent,
}

#[derive(Deserialize)]
struct BlanketContent {
	#[serde(default, deserialize_with = "ruma::serde::default_on_error")]
	default_action: Option<InvitePermissionAction>,
}

/// The recipient's verdict on a single invite from one sender.
///
/// Fetches the filtering state and discards it, so a caller judging several
/// senders should hold an [`InviteFilter`] instead.
#[implement(super::Service)]
pub async fn invite_permission(&self, sender: &UserId, recipient: &UserId) -> InvitePermission {
	self.invite_filter(recipient)
		.await
		.permission(sender)
}

/// The recipient's verdict on an invite from `sender`.
///
/// A blanket block outranks everything, since the invite-permission module
/// requires the 403 even when the sender is also on the ignore list. The
/// ignored-user list comes next, then the rule lists slot by slot, and the
/// first verdict other than allow wins.
#[implement(InviteFilter)]
#[must_use]
pub fn permission(&self, sender: &UserId) -> InvitePermission {
	match self {
		| _ if self.blocks_all() => InvitePermission::Block,
		| _ if self.ignores(sender) => InvitePermission::Ignore,
		| _ => self
			.configs()
			.map(|config| config.permission(sender))
			.find(|permission| permission.ne(&InvitePermission::Allow))
			.unwrap_or(InvitePermission::Allow),
	}
}

/// Whether the recipient accepts no invites at all.
///
/// Any slot carrying the blanket switch decides this, so a client writing
/// one slot is never undone by another slot left behind.
#[implement(InviteFilter)]
#[inline]
fn blocks_all(&self) -> bool {
	self.configs()
		.any(|config| config.default_action == Some(InvitePermissionAction::Block))
}

/// Whether the recipient ignores `sender` account-wide.
///
/// This is the ordinary `m.ignored_user_list`, which the invite-permission
/// module defers to for everything short of a blanket block.
#[implement(InviteFilter)]
#[inline]
fn ignores(&self, sender: &UserId) -> bool {
	self.ignored
		.as_ref()
		.is_some_and(|content| content.ignored_users.contains_key(sender))
}

/// Whether the recipient configured no filtering at all.
///
/// Callers on the sync path lean on this to skip deriving an invite's sender,
/// which costs a stripped-state load they would otherwise pay per room.
#[implement(InviteFilter)]
#[inline]
#[must_use]
pub fn is_permissive(&self) -> bool {
	self.ignored
		.as_ref()
		.is_none_or(|ignored| ignored.ignored_users.is_empty())
		&& self
			.configs()
			.all(InvitePermissionConfigEventContent::is_inert)
}

/// The configuration slots in evaluation order.
#[implement(InviteFilter)]
#[inline]
fn configs(&self) -> impl Iterator<Item = &InvitePermissionConfigEventContent> + Send {
	self.stable
		.iter()
		.chain(self.unstable_blanket.iter())
		.chain(self.unstable_lists.iter())
}

#[cfg(test)]
mod tests {
	use ruma::{
		events::invite_permission_config::UnstableInvitePermissionConfigEventContent, user_id,
	};
	use serde_json::{from_value, json};

	use super::{InviteFilter, InvitePermission};

	fn filter(
		ignored: Option<serde_json::Value>,
		stable: Option<serde_json::Value>,
		unstable_lists: Option<serde_json::Value>,
	) -> InviteFilter {
		InviteFilter {
			ignored: ignored.map(|content| from_value(content).unwrap()),
			stable: stable.map(|content| from_value(content).unwrap()),
			unstable_blanket: None,
			unstable_lists: unstable_lists.map(|content| from_value(content).unwrap()),
		}
	}

	fn blanket_filter(block_all: bool) -> InviteFilter {
		InviteFilter {
			ignored: None,
			stable: None,
			unstable_blanket: Some(
				UnstableInvitePermissionConfigEventContent::new(block_all).into(),
			),
			unstable_lists: None,
		}
	}

	#[test]
	fn blanket_block_outranks_ignored_list() {
		let sender = user_id!("@alice:example.org");
		let ignored = json!({"ignored_users": {"@alice:example.org": {}}});

		let both = filter(Some(ignored.clone()), Some(json!({"default_action": "block"})), None);

		assert_eq!(both.permission(sender), InvitePermission::Block);

		let ignored_only = filter(Some(ignored), None, None);

		assert_eq!(ignored_only.permission(sender), InvitePermission::Ignore);
	}

	#[test]
	fn vacuous_configuration_is_permissive() {
		let empty_list = filter(Some(json!({"ignored_users": {}})), None, None);

		assert!(empty_list.is_permissive());

		let empty_config = filter(None, Some(json!({})), Some(json!({"allowed_users": []})));

		assert!(empty_config.is_permissive());

		let disabled =
			filter(None, None, Some(json!({"enabled": false, "blocked_servers": ["*"]})));

		assert!(disabled.is_permissive());

		let filtering = filter(None, None, Some(json!({"blocked_servers": ["*"]})));

		assert!(!filtering.is_permissive());
		assert!(!blanket_filter(true).is_permissive());
	}

	#[test]
	fn unstable_blanket_slot_blocks() {
		let sender = user_id!("@alice:example.org");

		assert_eq!(blanket_filter(true).permission(sender), InvitePermission::Block);
		assert_eq!(blanket_filter(false).permission(sender), InvitePermission::Allow);
		assert!(blanket_filter(false).is_permissive());
	}

	#[test]
	fn first_non_allow_verdict_spans_both_slots() {
		let sender = user_id!("@alice:example.org");

		let cross = filter(
			None,
			Some(json!({"allowed_users": ["@alice:example.org"]})),
			Some(json!({"blocked_servers": ["*"]})),
		);

		assert_eq!(cross.permission(sender), InvitePermission::Block);

		let unstable_only = filter(None, None, Some(json!({"ignored_users": ["@alice:*"]})));

		assert_eq!(unstable_only.permission(sender), InvitePermission::Ignore);
	}
}
