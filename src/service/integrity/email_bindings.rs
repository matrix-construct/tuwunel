//! Email bindings a foreign database keeps in a column of its own.
//!
//! Some databases hold one address per localpart there rather than in the
//! third-party identifier store this server reads, so until the binding is
//! adopted the address looks unclaimed here.

use std::{fmt, str::from_utf8, sync::Arc};

use futures::{FutureExt, Stream, TryStreamExt, future::BoxFuture, stream::once};
use ruma::{OwnedUserId, ServerName, UserId};
use tuwunel_core::{
	Result,
	result::NotFound,
	smallstr::SmallString,
	utils::{BoolExt, IterStream, TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::{KeyBuf, Map};

use super::{Check, Report, ScanExt, Summary, leading_record, tally};
use crate::{
	Services,
	migrations::{ADOPT_EMAIL_BINDINGS, local_user_id, marker_present},
	threepid::canonicalize_email,
};

type Text = SmallString<[u8; 48]>;

type Column = Option<Arc<Map>>;

type Findings = (Option<Finding>, Option<Finding>);

pub(super) static CHECK: Check = Check {
	name: "foreign-email-bindings",
	about: "Reports only while the foreign email binding adoption is unstamped: every address a \
	        foreign column binds to an account here other than the server's own is bound to \
	        that account",
	quiescent: true,
	walk,
};

const COLUMN: &str = "localpart_email";

/// A foreign binding whose address is not bound to its account here, or a row
/// this check cannot read.
///
/// The adoption binds each address held by no account, and leaves one held by
/// another account, which the store cannot bind twice.
pub enum Finding {
	/// The address is bound to no account here.
	Unadopted {
		/// The account the binding names.
		user_id: OwnedUserId,
	},

	/// The address is bound to another account here.
	Claimed {
		/// The account the binding names.
		user_id: OwnedUserId,

		/// The account holding the address.
		owner: OwnedUserId,
	},

	/// The localpart forms no user id here.
	UnusableLocalpart {
		/// The stored localpart.
		localpart: Text,
	},

	/// The address is not one an account can be bound to.
	UnusableAddress {
		/// The account the binding names.
		user_id: OwnedUserId,
	},

	/// The row is not text.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},

	/// The account holding the address here does not decode.
	UnparsableOwner {
		/// The account the binding names.
		user_id: OwnedUserId,
	},

	/// The row holds records past its localpart and address, which the adoption
	/// ignores.
	ExtraRecords {
		/// The account the binding names.
		user_id: OwnedUserId,
	},
}

/// One row of the foreign binding column, decoded.
///
/// The check and the adoption walk the same rows, so they agree on which
/// bindings the column holds.
pub(crate) enum Row {
	/// The account the localpart names here, with the address bound to it.
	Binding {
		/// The account the localpart names.
		user_id: OwnedUserId,

		/// The address as stored.
		address: Text,

		/// Whether the row holds records past its localpart and address.
		extra: bool,
	},

	/// The localpart forms no user id here.
	UnusableLocalpart {
		/// The stored localpart.
		localpart: Text,
	},

	/// The row is not text.
	Unparsable {
		/// The stored key.
		key: KeyBuf,
	},
}

/// What a foreign binding's account and address are here, as the adoption reads
/// them.
///
/// Only a binding with an account and a usable address goes on to be bound.
pub(crate) enum Bindable {
	/// No account holds the localpart here, or it is the server's own.
	NoAccount,

	/// The address does not canonicalize.
	UnusableAddress,

	/// The address in its canonical form.
	Canonical(String),
}

/// Opens the foreign binding column, if the database holds one.
///
/// A database no foreign server has opened holds none, which is no defect.
pub fn column(services: &Services) -> Result<Column> { services.db.open_cf(COLUMN) }

/// Streams each foreign binding whose address is not bound to its account here,
/// only while the adoption is unstamped.
///
/// A row holding records past its shape, or one this check cannot read, is
/// reported too.
/// Once the adoption is stamped, local administration owns the bindings and the
/// foreign column no longer describes them, so nothing is reported. A binding
/// naming no account here is never reported. Only the reverse row resolving the
/// address is read, not the account's own listing.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan<'a>(
	services: &'a Services,
	localpart_email: &'a Arc<Map>,
) -> impl Stream<Item = Result<Finding>> + Send + 'a {
	once(marker_present(services, ADOPT_EMAIL_BINDINGS))
		.map_ok(move |adopted| {
			adopted
				.is_false()
				.then(|| rows(services, localpart_email))
		})
		.ready_try_filter_map(Ok)
		.try_flatten()
		.broad_and_then(|row| findings(services, row))
		.map_ok(|(verdict, extra)| verdict.into_iter().chain(extra).try_stream())
		.try_flatten()
}

/// Streams every row of the foreign binding column, decoded.
///
/// The localpart and the address are each read from their leading record, as a
/// release build decodes them. Nothing is buffered, so a pass consuming the rows one at a time decides each
/// binding after its writes for earlier ones, as when two addresses fold onto
/// one canonical form.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn rows<'a>(
	services: &'a Services,
	localpart_email: &'a Arc<Map>,
) -> impl Stream<Item = Result<Row>> + Send + 'a {
	let server_name = services.globals.server_name();

	localpart_email
		.raw_scan()
		.scanned(&services.server)
		.ready_and_then(move |(key, value)| Ok(row(key, value, server_name)))
}

/// Reads whether a binding's account is one an address can be bound to, and the
/// address's canonical form.
///
/// A deactivated account still reserves its address, since neither server
/// unhooks a binding on deactivation. Only a failed read is an error.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn bindable(
	services: &Services,
	user_id: &UserId,
	address: &str,
) -> Result<Bindable> {
	match services.db["userid_password"].get(user_id).await {
		| Ok(_) if user_id != services.globals.server_user => (),
		| Ok(_) => return Ok(Bindable::NoAccount),
		| Err(e) if e.is_not_found() => return Ok(Bindable::NoAccount),
		| Err(e) => return Err(e),
	}

	let bindable = canonicalize_email(address)
		.map(Bindable::Canonical)
		.unwrap_or(Bindable::UnusableAddress);

	Ok(bindable)
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Unadopted { .. } => "unadopted-binding",
			| Self::Claimed { .. } => "claimed-elsewhere",
			| Self::UnusableLocalpart { .. } => "unusable-localpart",
			| Self::UnusableAddress { .. } => "unusable-address",
			| Self::Unparsable { .. } => "unparsable-binding",
			| Self::UnparsableOwner { .. } => "unparsable-owner",
			| Self::ExtraRecords { .. } => "extra-records",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Unadopted { user_id } => {
				write!(
					f,
					"the address a foreign column binds to {user_id} is bound to no one here"
				)
			},
			| Self::Claimed { user_id, owner } => write!(
				f,
				"the address a foreign column binds to {user_id} is bound to {owner} here"
			),
			| Self::UnusableLocalpart { localpart } => {
				write!(f, "foreign binding localpart {localpart:?} forms no user id here")
			},
			| Self::UnusableAddress { user_id } => {
				write!(f, "the address a foreign column binds to {user_id} is not an address")
			},
			| Self::Unparsable { key } => {
				write!(f, "foreign binding {} is not text", key.escape_ascii())
			},
			| Self::UnparsableOwner { user_id } => write!(
				f,
				"the account holding the address a foreign column binds to {user_id} does not \
				 decode"
			),
			| Self::ExtraRecords { user_id } => {
				write!(f, "the foreign binding of {user_id} holds records past its shape")
			},
		}
	}
}

// The opened column lives in this future, since the walk borrows it.
fn walk(services: &Services, limit: usize) -> BoxFuture<'_, Result<Summary>> {
	async move {
		let Some(localpart_email) = column(services)? else {
			return Ok(Summary::default());
		};

		tally(scan(services, &localpart_email), limit).await
	}
	.boxed() // type-erasure boundary
}

#[tracing::instrument(level = "debug", skip_all)]
async fn findings(services: &Services, row: Row) -> Result<Findings> {
	let (user_id, address, extra) = match row {
		| Row::Binding { user_id, address, extra } => (user_id, address, extra),
		| Row::UnusableLocalpart { localpart } => {
			return Ok((Some(Finding::UnusableLocalpart { localpart }), None));
		},
		| Row::Unparsable { key } => return Ok((Some(Finding::Unparsable { key }), None)),
	};

	let extra = extra.then(|| Finding::ExtraRecords { user_id: user_id.clone() });
	let verdict = verdict(services, user_id, &address).await?;

	Ok((verdict, extra))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn verdict(
	services: &Services,
	user_id: OwnedUserId,
	address: &str,
) -> Result<Option<Finding>> {
	let email_canon = match bindable(services, &user_id, address).await? {
		| Bindable::NoAccount => return Ok(None),
		| Bindable::UnusableAddress => return Ok(Some(Finding::UnusableAddress { user_id })),
		| Bindable::Canonical(email_canon) => email_canon,
	};

	let bound = services.db["email_userid"]
		.get(&email_canon)
		.await
		.optional()?;

	let finding = match bound.as_deref().map(owner) {
		| None => Some(Finding::Unadopted { user_id }),
		| Some(Some(owner)) if owner == user_id => None,
		| Some(Some(owner)) => Some(Finding::Claimed { user_id, owner: owner.to_owned() }),
		| Some(None) => Some(Finding::UnparsableOwner { user_id }),
	};

	Ok(finding)
}

fn row(key: &[u8], value: &[u8], server_name: &ServerName) -> Row {
	let ((localpart, key_rest), (address, value_rest)) =
		(leading_record(key), leading_record(value));

	let (Ok(localpart), Ok(address)) = (from_utf8(localpart), from_utf8(address)) else {
		return Row::Unparsable { key: KeyBuf::from_slice(key) };
	};

	match local_user_id(localpart, server_name) {
		| Some(user_id) => Row::Binding {
			user_id,
			address: address.into(),
			extra: key_rest.or(value_rest).is_some(),
		},
		| None => Row::UnusableLocalpart { localpart: localpart.into() },
	}
}

fn owner(value: &[u8]) -> Option<&UserId> {
	from_utf8(leading_record(value).0)
		.ok()
		.and_then(|owner| <&UserId>::try_from(owner).ok())
}
