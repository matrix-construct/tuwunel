use std::collections::BTreeMap;

use ruma::{OwnedUserId, UserId};
use serde_json::Value;
use tuwunel_core::{debug, implement, trace};

use super::{Sessions, UserInfo};

pub(super) type Pending = BTreeMap<String, Claimants>;
type Claimants = BTreeMap<OwnedUserId, Claims>;
pub type Claims = BTreeMap<String, String>;

#[implement(Sessions)]
pub fn set_user_association_pending(
	&self,
	idp_id: &str,
	user_id: &UserId,
	claims: Claims,
) -> Option<Claims> {
	self.association_pending
		.lock()
		.expect("locked")
		.entry(idp_id.into())
		.or_default()
		.insert(user_id.into(), claims)
}

#[implement(Sessions)]
pub fn find_user_association_pending(
	&self,
	idp_id: &str,
	userinfo: &UserInfo,
) -> Option<OwnedUserId> {
	let claiming = serde_json::to_value(userinfo)
		.expect("Failed to transform user_info into serde_json::Value");

	let claiming = claiming
		.as_object()
		.expect("Failed to interpret user_info as object");

	assert!(
		!claiming.is_empty(),
		"Expecting at least one claim from user_info such as `sub`"
	);

	debug!(?idp_id, ?claiming, "finding pending association",);
	self.association_pending
		.lock()
		.expect("locked")
		.get(idp_id)
		.into_iter()
		.flat_map(Claimants::iter)
		.find_map(|(user_id, claimant)| {
			trace!(?user_id, ?claimant, "checking against pending association");

			assert!(
				!claimant.is_empty(),
				"Must not match empty set of claims; should not exist in association_pending."
			);

			for (claim, value) in claimant {
				if claiming.get(claim).and_then(Value::as_str) != Some(value) {
					return None;
				}
			}

			Some(user_id.clone())
		})
}

#[implement(Sessions)]
pub fn remove_provider_associations_pending(&self, idp_id: &str) {
	self.association_pending
		.lock()
		.expect("locked")
		.remove(idp_id);
}

#[implement(Sessions)]
pub fn remove_user_association_pending(&self, user_id: &UserId, idp_id: Option<&str>) {
	self.association_pending
		.lock()
		.expect("locked")
		.iter_mut()
		.filter(|(provider, _)| idp_id == Some(provider))
		.for_each(|(_, claiming)| {
			claiming.remove(user_id);
		});
}

#[implement(Sessions)]
pub fn is_user_association_pending(&self, user_id: &UserId) -> bool {
	self.association_pending
		.lock()
		.expect("locked")
		.values()
		.any(|claiming| claiming.contains_key(user_id))
}
