use std::sync::Arc;

use futures::TryStreamExt;
use ruma::{OwnedUserId, UserId};
use tuwunel_core::{
	Result, err, info,
	result::NotFound,
	utils::{ReadyExt, option::OptionExt, stream::BroadbandExt},
	warn,
};
use tuwunel_database::Map;

use super::local_user_id;
use crate::{
	Services,
	users::{PASSWORD_DISABLED, PASSWORD_SENTINEL},
};

/// Reconciles account states a foreign database keeps outside the password
/// column.
///
/// Some databases mark deactivation in a column of its own while leaving the
/// hash in place, and spell an account authenticated elsewhere as an empty
/// hash. This server reads both states from the password alone, so until they
/// are adopted a deactivated account reads as active and an externally
/// authenticated one reads as deactivated. Adoption happens once, because
/// local administration writes the same column afterward and a second pass
/// would undo it.
pub(super) async fn migrate_account_status(services: &Services) -> Result {
	let deactivated = services.db.open_cf("userid_deactivated")?;
	let subjects = services.db.open_cf("openidsubject_localpart")?;

	if let Some(deactivated) = deactivated.as_ref() {
		adopt_deactivations(services, deactivated).await?;
	}

	if let Some(subjects) = subjects.as_ref() {
		adopt_passwordless(services, subjects, deactivated.as_ref()).await?;
	}

	Ok(())
}

/// Empties the password of every account a foreign column marks deactivated.
///
/// The marker is invisible here while the surviving hash reads as an active
/// account, restoring a login the origin had already withdrawn. An empty
/// password is the same state spelled locally, and costs the foreign hash,
/// which decides nothing for an account deactivated on both sides.
async fn adopt_deactivations(services: &Services, deactivated: &Arc<Map>) -> Result {
	let userid_password = &services.db["userid_password"];
	let cork = services.db.cork_and_sync();

	let (adopted, unreadable) = deactivated
		.keys::<&UserId>()
		.map_ok(ToOwned::to_owned)
		.broad_filter_map(async |account: Result<OwnedUserId>| {
			let user_id = match account {
				| Ok(user_id) => user_id,
				| Err(e) => return Some(Err(e)),
			};

			match hash_empty(userid_password, &user_id).await {
				| Ok(Some(false)) => Some(Ok(user_id)),
				| Ok(_) => None,
				| Err(e) => Some(Err(e)),
			}
		})
		.ready_fold((0_usize, 0_usize), |counts, account| {
			write_password(userid_password, PASSWORD_DISABLED, counts, account)
		})
		.await;

	drop(cork);

	if adopted > 0 {
		info!(%adopted, "Adopted deactivated accounts from a foreign database");
	}

	unreadable
		.eq(&0)
		.then_some(())
		.ok_or_else(|| err!(Database("{unreadable} accounts could not be read")))
}

/// Restores the sentinel password on accounts an identity provider
/// authenticates.
///
/// A foreign database spells "no local password" as an empty hash, which reads
/// here as deactivated and refuses the account every login flow. Only accounts
/// carrying a provider subject are restored, because an empty hash on its own
/// cannot be told apart from a deactivation this server wrote.
///
/// The sentinel carries that meaning locally, leaving the account active with
/// no password to verify against, while an account the foreign column marks
/// deactivated keeps its deactivation.
async fn adopt_passwordless(
	services: &Services,
	subjects: &Arc<Map>,
	deactivated: Option<&Arc<Map>>,
) -> Result {
	let userid_password = &services.db["userid_password"];
	let server_name = services.globals.server_name();
	let cork = services.db.cork_and_sync();

	let (adopted, unreadable) = subjects
		.stream()
		.ready_filter_map(|subject: Result<(&str, &str)>| match subject {
			| Ok((_, localpart)) => local_user_id(localpart, server_name).map(Ok),
			| Err(e) => Some(Err(e)),
		})
		.broad_filter_map(async |account: Result<OwnedUserId>| {
			let user_id = match account {
				| Ok(user_id) => user_id,
				| Err(e) => return Some(Err(e)),
			};

			match restorable(services, deactivated, &user_id).await {
				| Ok(false) => None,
				| Ok(true) => Some(Ok(user_id)),
				| Err(e) => Some(Err(e)),
			}
		})
		.ready_fold((0_usize, 0_usize), |counts, account| {
			write_password(userid_password, PASSWORD_SENTINEL, counts, account)
		})
		.await;

	drop(cork);

	if adopted > 0 {
		info!(%adopted, "Restored accounts authenticated elsewhere from a foreign database");
	}

	unreadable
		.eq(&0)
		.then_some(())
		.ok_or_else(|| err!(Database("{unreadable} accounts could not be read")))
}

/// Reports whether the account reads as deactivated here without the foreign
/// column marking it so.
///
/// The empty password a foreign database gives an account authenticated
/// elsewhere is the byte pattern this server writes for a deactivation, so the
/// foreign marker is the only thing separating them. A row neither side can
/// read is reported rather than guessed at.
async fn restorable(
	services: &Services,
	deactivated: Option<&Arc<Map>>,
	user_id: &UserId,
) -> Result<bool> {
	let userid_password = &services.db["userid_password"];
	let passwordless = hash_empty(userid_password, user_id)
		.await?
		.is_some_and(|empty| empty);

	let marked = match deactivated
		.map_async(|deactivated| deactivated.exists(user_id))
		.await
	{
		| None => false,
		| Some(Ok(())) => true,
		| Some(Err(e)) if e.is_not_found() => false,
		| Some(Err(e)) => return Err(e),
	};

	Ok(passwordless && !marked)
}

/// Whether the account's stored password is empty, or `None` when it has no
/// row at all.
///
/// Both folds need the three states kept apart: a read failure is neither an
/// active account nor a deactivated one, and mistaking it for either is how a
/// pass that runs once leaves an account in the wrong state for good.
async fn hash_empty(userid_password: &Arc<Map>, user_id: &UserId) -> Result<Option<bool>> {
	userid_password
		.get(user_id)
		.await
		.map(|hash| hash.is_empty())
		.optional()
}

/// Writes one adopted account, tallying it against the rows that could not be
/// read.
fn write_password(
	userid_password: &Arc<Map>,
	password: &str,
	(adopted, unreadable): (usize, usize),
	account: Result<OwnedUserId>,
) -> (usize, usize) {
	match account {
		| Ok(user_id) => {
			userid_password.insert(&user_id, password);

			(adopted.saturating_add(1), unreadable)
		},
		| Err(e) => {
			warn!(error = %e, "an account could not be read");

			(adopted, unreadable.saturating_add(1))
		},
	}
}
