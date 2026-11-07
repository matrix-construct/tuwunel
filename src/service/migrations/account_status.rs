use std::sync::Arc;

use futures::TryStreamExt;
use ruma::OwnedUserId;
use tuwunel_core::{Error, Result, err, info, warn};
use tuwunel_database::{KeyBuf, Map};

use crate::{
	Services,
	integrity::account_status::{
		Columns, Named, Subject, columns, deactivations, hash_kept, subjects,
		unmarked_passwordless,
	},
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
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn migrate_account_status(services: &Services) -> Result {
	let Columns { deactivated, subjects } = columns(services)?;

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
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn adopt_deactivations(services: &Services, deactivated: &Arc<Map>) -> Result {
	let userid_password = &services.db["userid_password"];
	let cork = services.db.cork_and_sync();

	// One account at a time, each decided against its row as it is written.
	let (adopted, unreadable) = deactivations(services, deactivated)
		.try_fold((0_usize, 0_usize), async |counts, account| {
			let adoptable = match account {
				| Ok(Named { user_id, .. }) => hash_kept(services, &user_id)
					.await
					.map(|kept| kept.then_some(user_id)),
				| Err(key) => Err(unparsable("deactivated account", &key)),
			};

			Ok(write_password(userid_password, PASSWORD_DISABLED, counts, adoptable))
		})
		.await?;

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
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn adopt_passwordless(
	services: &Services,
	subjects_column: &Arc<Map>,
	deactivated: Option<&Arc<Map>>,
) -> Result {
	let userid_password = &services.db["userid_password"];
	let cork = services.db.cork_and_sync();

	// One account at a time, each decided against its row as it is written.
	let (adopted, unreadable) = subjects(services, subjects_column)
		.try_fold((0_usize, 0_usize), async |counts, subject| {
			let adoptable = match subject {
				| Subject::UnusableLocalpart { .. } => Ok(None),
				| Subject::Account(Named { user_id, .. }) =>
					unmarked_passwordless(services, deactivated, &user_id)
						.await
						.map(|due| due.then_some(user_id)),
				| Subject::Unparsable { key } => Err(unparsable("provider subject", &key)),
			};

			Ok(write_password(userid_password, PASSWORD_SENTINEL, counts, adoptable))
		})
		.await?;

	drop(cork);

	if adopted > 0 {
		info!(%adopted, "Restored accounts authenticated elsewhere from a foreign database");
	}

	unreadable
		.eq(&0)
		.then_some(())
		.ok_or_else(|| err!(Database("{unreadable} accounts could not be read")))
}

/// Writes one adoptable account, tallying it against the rows that could not be
/// read.
///
/// An account the adoption leaves alone is not counted, while a row that could
/// not be read is logged and withholds the marker.
fn write_password(
	userid_password: &Arc<Map>,
	password: &str,
	(adopted, unreadable): (usize, usize),
	account: Result<Option<OwnedUserId>>,
) -> (usize, usize) {
	match account {
		| Ok(None) => (adopted, unreadable),
		| Ok(Some(user_id)) => {
			userid_password.insert(&user_id, password);

			(adopted.saturating_add(1), unreadable)
		},
		| Err(e) => {
			warn!(error = %e, "an account could not be read");

			(adopted, unreadable.saturating_add(1))
		},
	}
}

fn unparsable(row: &str, key: &KeyBuf) -> Error {
	let key = key.escape_ascii();

	err!(Database("{row} {key} could not be read"))
}
