use futures::StreamExt;
use ruma::{MilliSecondsSinceUnixEpoch, ServerName, thirdparty::Medium};
use tuwunel_core::{Result, debug_warn, err, info, warn};

use super::local_user_id;
use crate::{Services, threepid::canonicalize_email};

/// Adopts the email addresses a foreign database binds in a column of its own.
///
/// Some databases hold one address per localpart in their own column rather
/// than in the third-party identifier store this server reads, so the binding
/// is invisible here and the address looks unclaimed. Adopting it keeps the
/// address answering for its owner, which is what an account authenticated by
/// an identity provider carries instead of a password.
///
/// Addresses are taken one at a time because two foreign addresses can fold
/// onto one canonical key, and a concurrent pass would clear both through the
/// in-use check before either wrote the reverse row.
pub(super) async fn migrate_email_bindings(services: &Services) -> Result {
	let Some(localpart_email) = services.db.open_cf("localpart_email")? else {
		return Ok(());
	};

	let server_name = services.globals.server_name();
	let bound_at = MilliSecondsSinceUnixEpoch::now();
	let cork = services.db.cork_and_sync();

	let (adopted, skipped, unreadable) = localpart_email
		.stream()
		.fold((0_usize, 0_usize, 0_usize), async |acc, binding: Result<(&str, &str)>| {
			let adopted = match binding {
				| Err(e) => Err(e),
				| Ok((localpart, address)) =>
					adopt_one(services, server_name, bound_at, localpart, address).await,
			};

			tally_adoption(acc, adopted)
		})
		.await;

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
	server_name: &ServerName,
	bound_at: MilliSecondsSinceUnixEpoch,
	localpart: &str,
	address: &str,
) -> Result<bool> {
	let Some(user_id) = local_user_id(localpart, server_name) else {
		debug_warn!(%localpart, "skipping an unusable localpart");
		return Ok(false);
	};

	// A deactivated account still reserves its address, since neither server
	// unhooks a binding on deactivation.
	match services.db["userid_password"].get(&user_id).await {
		| Ok(_) if user_id != services.globals.server_user => (),
		| Ok(_) => return Ok(false),
		| Err(e) if e.is_not_found() => return Ok(false),
		| Err(e) => return Err(e),
	}

	let Ok(email_canon) = canonicalize_email(address) else {
		debug_warn!(%localpart, "skipping an unusable address");
		return Ok(false);
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
