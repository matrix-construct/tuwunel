use std::sync::Arc;

use futures::TryStreamExt;
use ruma::{MilliSecondsSinceUnixEpoch, thirdparty::Medium};
use tuwunel_core::{Err, Result, debug_warn, err, info, warn};
use tuwunel_database::Map;

use crate::{
	Services,
	integrity::email_bindings::{Bindable, Row, bindable, column, rows},
};

/// Adopts the email addresses a foreign database binds in a column of its own.
///
/// Some databases hold one address per localpart in their own column rather
/// than in the third-party identifier store this server reads, so the binding
/// is invisible here and the address looks unclaimed. Adopting it keeps the
/// address answering for its owner, which is what an account authenticated by
/// an identity provider carries instead of a password.
pub(super) async fn migrate_email_bindings(services: &Services) -> Result {
	let Some(localpart_email) = column(services)? else {
		return Ok(());
	};

	adopt_email_bindings(services, &localpart_email).await
}

/// Adopts every binding an opened foreign column holds.
///
/// Addresses are taken one at a time because two foreign addresses can fold
/// onto one canonical key, and a concurrent pass would clear both through the
/// in-use check before either wrote the reverse row.
pub(super) async fn adopt_email_bindings(
	services: &Services,
	localpart_email: &Arc<Map>,
) -> Result {
	let bound_at = MilliSecondsSinceUnixEpoch::now();
	let cork = services.db.cork_and_sync();

	let (adopted, skipped, unreadable) = rows(services, localpart_email)
		.try_fold((0_usize, 0_usize, 0_usize), async |acc, row| {
			Ok(tally_adoption(acc, adopt_one(services, bound_at, row).await))
		})
		.await?;

	drop(cork);

	match skipped {
		| 0 if adopted > 0 => info!(%adopted, "Adopted email bindings from a foreign database"),
		| 0 => (),
		| _ => warn!(
			%adopted,
			%skipped,
			"Adopted email bindings from a foreign database; some addresses were left behind"
		),
	}

	// Leaving the marker unstamped is what makes a read failure recoverable: the
	// pass is idempotent, so the next boot retries it whole.
	unreadable
		.eq(&0)
		.then_some(())
		.ok_or_else(|| err!(Database("{unreadable} email bindings could not be read")))
}

/// Binds one foreign row, reporting whether it produced a binding.
///
/// A `false` return is a row with nothing to bind: an unusable localpart or
/// address, an account absent here, the server's own, or an address already
/// held by a different account, which this store cannot represent twice. An
/// error is a read that failed and must not be mistaken for any of them.
async fn adopt_one(
	services: &Services,
	bound_at: MilliSecondsSinceUnixEpoch,
	row: Row,
) -> Result<bool> {
	let (user_id, address) = match row {
		| Row::Binding { user_id, address, .. } => (user_id, address),
		| Row::UnusableLocalpart { localpart } => {
			debug_warn!(%localpart, "skipping an unusable localpart");
			return Ok(false);
		},
		| Row::Unparsable { key } => {
			let key = key.escape_ascii();

			return Err!(Database("email binding {key} is not text"));
		},
	};

	let email_canon = match bindable(services, &user_id, &address).await? {
		| Bindable::NoAccount => return Ok(false),
		| Bindable::Canonical(email_canon) => email_canon,
		| Bindable::UnusableAddress => {
			debug_warn!(localpart = %user_id.localpart(), "skipping an unusable address");
			return Ok(false);
		},
	};

	services
		.threepid
		.bind_unclaimed(&user_id, &email_canon, Medium::Email, bound_at)
		.await
}

fn tally_adoption(
	(adopted, skipped, unreadable): (usize, usize, usize),
	result: Result<bool>,
) -> (usize, usize, usize) {
	match result {
		| Ok(true) => (adopted.saturating_add(1), skipped, unreadable),
		| Ok(false) => (adopted, skipped.saturating_add(1), unreadable),
		| Err(e) => {
			warn!(error = %e, "an email binding could not be read");

			(adopted, skipped, unreadable.saturating_add(1))
		},
	}
}
