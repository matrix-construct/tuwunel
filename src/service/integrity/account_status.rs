//! Account states a foreign database keeps outside the password column.
//!
//! Some databases mark deactivation in a column of its own while leaving the
//! hash in place, and spell an account an identity provider authenticates as an
//! empty hash. This server reads both states from the password alone, so until
//! they are adopted the first reads as active and the second as deactivated.

use std::{fmt, str::from_utf8, sync::Arc};

use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::BoxFuture, stream::once,
};
use ruma::{OwnedUserId, ServerName, UserId};
use tuwunel_core::{
	Result,
	result::NotFound,
	smallstr::SmallString,
	utils::{BoolExt, IterStream, TryReadyExt, option::OptionExt, stream::TryBroadbandExt},
};
use tuwunel_database::{KeyBuf, Map};

use super::{Check, Report, ScanExt, Summary, leading_record, tally};
use crate::{
	Services,
	migrations::{ADOPT_ACCOUNT_STATUS, local_user_id, marker_present},
};

type Findings = (Option<Finding>, Option<Finding>);

type Text = SmallString<[u8; 48]>;

pub(super) static CHECK: Check = Check {
	name: "foreign-account-status",
	about: "Reports only while the foreign account status adoption is unstamped: every account \
	        here a foreign column marks deactivated has an empty password, and every one a \
	        foreign provider subject names has a non-empty one unless so marked",
	quiescent: true,
	walk,
};

const DEACTIVATED_COLUMN: &str = "userid_deactivated";

const SUBJECTS_COLUMN: &str = "openidsubject_localpart";

/// The foreign account columns the database holds, opened.
///
/// A walk borrows the columns it reads, so whoever walks them holds these for as
/// long as the walk runs.
pub struct Columns {
	/// The column marking deactivated accounts, keyed by user id.
	pub deactivated: Option<Arc<Map>>,

	/// The column naming the localpart each provider subject authenticates.
	pub subjects: Option<Arc<Map>>,
}

/// An account whose password here does not say what a foreign column records,
/// or a foreign row this check cannot read.
///
/// The adoption empties the password of a marked account and gives an unmarked
/// passwordless one the sentinel.
pub enum Finding {
	/// A foreign column marks the account deactivated, while its password here is
	/// not empty.
	Deactivation {
		/// The account.
		user_id: OwnedUserId,
	},

	/// A foreign provider subject names the account, whose password here is empty
	/// while no foreign column marks it deactivated.
	Passwordless {
		/// The account.
		user_id: OwnedUserId,
	},

	/// A key of the deactivation column is not a user id.
	UnparsableAccount {
		/// The stored key.
		key: KeyBuf,
	},

	/// A row of the subject column is not text.
	UnparsableSubject {
		/// The stored key.
		key: KeyBuf,
	},

	/// A subject's localpart forms no user id here.
	UnusableLocalpart {
		/// The stored localpart.
		localpart: Text,
	},

	/// A foreign row naming the account holds records past its shape, which the
	/// adoption ignores.
	ExtraRecords {
		/// The account.
		user_id: OwnedUserId,
	},
}

/// An account the foreign deactivation column names, or the key of a row
/// naming none.
///
/// The key names the row in a finding, and the adoption counts such a row as
/// unreadable.
pub(crate) type Account = Result<Named, KeyBuf>;

/// The account a foreign row names, read as a release build decodes the row.
///
/// Records past the ones naming the account are ignored, which the flag reports.
pub(crate) struct Named {
	/// The account.
	pub(crate) user_id: OwnedUserId,

	/// Whether the row holds records past the ones naming the account.
	pub(crate) extra: bool,
}

/// What a row of the foreign subject column names.
///
/// The adoption skips a subject naming no account here without counting it, and
/// counts a row that is not text as unreadable.
pub(crate) enum Subject {
	/// The account the subject's localpart names here.
	Account(Named),

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

/// Opens each foreign account column the database holds.
///
/// A column the database lacks is left closed, since its absence is no defect.
pub fn columns(services: &Services) -> Result<Columns> {
	Ok(Columns {
		deactivated: services.db.open_cf(DEACTIVATED_COLUMN)?,
		subjects: services.db.open_cf(SUBJECTS_COLUMN)?,
	})
}

/// Streams each account whose password does not say what a foreign column
/// records, only while the adoption is unstamped.
///
/// A foreign row holding records past its shape, or one this check cannot read,
/// is reported too. Once the adoption is stamped, local administration owns the
/// password column and the foreign columns no longer describe it, so nothing is
/// reported. An account with no password row here is never reported.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan<'a>(
	services: &'a Services,
	columns: &'a Columns,
) -> impl Stream<Item = Result<Finding>> + Send + 'a {
	once(marker_present(services, ADOPT_ACCOUNT_STATUS))
		.map_ok(move |adopted| {
			adopted
				.is_false()
				.then(|| findings(services, columns))
		})
		.ready_try_filter_map(Ok)
		.try_flatten()
}

/// Streams every account the foreign deactivation column marks.
///
/// A key is read as a release build decodes it, as a user id in its leading
/// record with any more ignored, and one naming none is named by its key
/// instead. Nothing is buffered.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn deactivations<'a>(
	services: &'a Services,
	deactivated: &'a Arc<Map>,
) -> impl Stream<Item = Result<Account>> + Send + 'a {
	deactivated
		.raw_scan()
		.scanned(&services.server)
		.ready_and_then(|(key, _)| Ok(account(key)))
}

/// Streams what every row of the foreign subject column names.
///
/// Each part of a row is read from its leading record, as a release build
/// decodes it. Every row is yielded, including one whose localpart forms no user
/// id here and one that is not text. Nothing is buffered.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn subjects<'a>(
	services: &'a Services,
	subjects: &'a Arc<Map>,
) -> impl Stream<Item = Result<Subject>> + Send + 'a {
	let server_name = services.globals.server_name();

	subjects
		.raw_scan()
		.scanned(&services.server)
		.ready_and_then(move |(key, value)| Ok(subject(key, value, server_name)))
}

/// Whether the account has a password row here that is not empty.
///
/// That is how a foreign deactivation reads here while the foreign hash survives.
/// An account with no row is not one, and a failed read is an error.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn hash_kept(services: &Services, user_id: &UserId) -> Result<bool> {
	hash_empty(services, user_id)
		.map_ok(|empty| empty == Some(false))
		.await
}

/// Whether the account's password here is empty while no foreign column marks
/// it deactivated.
///
/// The empty password a foreign database gives an account authenticated
/// elsewhere is the byte pattern this server writes for a deactivation, so the
/// foreign marker is the only thing separating them. A row neither side can
/// read is reported rather than guessed at.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn unmarked_passwordless(
	services: &Services,
	deactivated: Option<&Arc<Map>>,
	user_id: &UserId,
) -> Result<bool> {
	let passwordless = hash_empty(services, user_id)
		.map_ok(|empty| empty == Some(true))
		.await?;

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

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Deactivation { .. } => "unadopted-deactivation",
			| Self::Passwordless { .. } => "unadopted-passwordless",
			| Self::UnparsableAccount { .. } => "unparsable-account",
			| Self::UnparsableSubject { .. } => "unparsable-subject",
			| Self::UnusableLocalpart { .. } => "unusable-localpart",
			| Self::ExtraRecords { .. } => "extra-records",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Deactivation { user_id } => {
				write!(f, "{user_id} is deactivated in a foreign column but has a password here")
			},
			| Self::Passwordless { user_id } => write!(
				f,
				"{user_id} is authenticated by a foreign provider but reads as deactivated here"
			),
			| Self::UnparsableAccount { key } => {
				write!(f, "foreign deactivation key {} is not a user id", key.escape_ascii())
			},
			| Self::UnparsableSubject { key } => {
				write!(f, "foreign provider subject {} is not text", key.escape_ascii())
			},
			| Self::UnusableLocalpart { localpart } => {
				write!(
					f,
					"foreign provider subject localpart {localpart:?} forms no user id here"
				)
			},
			| Self::ExtraRecords { user_id } => {
				write!(f, "a foreign row naming {user_id} holds records past its shape")
			},
		}
	}
}

// The opened columns live in this future, since the walk borrows them.
fn walk(services: &Services, limit: usize) -> BoxFuture<'_, Result<Summary>> {
	async move {
		let columns = columns(services)?;

		tally(scan(services, &columns), limit).await
	}
	.boxed() // type-erasure boundary
}

fn findings<'a>(
	services: &'a Services,
	columns: &'a Columns,
) -> impl Stream<Item = Result<Finding>> + Send + 'a {
	let deactivated = columns.deactivated.as_ref();

	let marked = deactivated
		.stream()
		.flat_map(move |column| deactivations(services, column))
		.broad_and_then(move |account| deactivation(services, account));

	let authenticated = columns
		.subjects
		.as_ref()
		.stream()
		.flat_map(move |column| subjects(services, column))
		.broad_and_then(move |subject| passwordless(services, deactivated, subject));

	marked
		.chain(authenticated)
		.map_ok(|(verdict, extra)| verdict.into_iter().chain(extra).try_stream())
		.try_flatten()
}

#[tracing::instrument(level = "debug", skip_all)]
async fn deactivation(services: &Services, account: Account) -> Result<Findings> {
	let Named { user_id, extra } = match account {
		| Ok(named) => named,
		| Err(key) => return Ok((Some(Finding::UnparsableAccount { key }), None)),
	};

	let kept = hash_kept(services, &user_id).await?;
	let extra = extra.then(|| Finding::ExtraRecords { user_id: user_id.clone() });
	let verdict = kept.then_some(Finding::Deactivation { user_id });

	Ok((verdict, extra))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn passwordless(
	services: &Services,
	deactivated: Option<&Arc<Map>>,
	subject: Subject,
) -> Result<Findings> {
	let Named { user_id, extra } = match subject {
		| Subject::Account(named) => named,
		| Subject::UnusableLocalpart { localpart } => {
			return Ok((Some(Finding::UnusableLocalpart { localpart }), None));
		},
		| Subject::Unparsable { key } =>
			return Ok((Some(Finding::UnparsableSubject { key }), None)),
	};

	let passwordless = unmarked_passwordless(services, deactivated, &user_id).await?;
	let extra = extra.then(|| Finding::ExtraRecords { user_id: user_id.clone() });
	let verdict = passwordless.then_some(Finding::Passwordless { user_id });

	Ok((verdict, extra))
}

fn account(key: &[u8]) -> Account {
	let (user_id, rest) = leading_record(key);

	from_utf8(user_id)
		.ok()
		.and_then(|user_id| <&UserId>::try_from(user_id).ok())
		.map(|user_id| Named {
			user_id: user_id.to_owned(),
			extra: rest.is_some(),
		})
		.ok_or_else(|| KeyBuf::from_slice(key))
}

fn subject(key: &[u8], value: &[u8], server_name: &ServerName) -> Subject {
	let ((subject, key_rest), (localpart, value_rest)) =
		(leading_record(key), leading_record(value));

	let (Ok(_), Ok(localpart)) = (from_utf8(subject), from_utf8(localpart)) else {
		return Subject::Unparsable { key: KeyBuf::from_slice(key) };
	};

	match local_user_id(localpart, server_name) {
		| Some(user_id) => Subject::Account(Named {
			user_id,
			extra: key_rest.or(value_rest).is_some(),
		}),
		| None => Subject::UnusableLocalpart { localpart: localpart.into() },
	}
}

/// Whether the account's stored password is empty, or `None` when it has no
/// row at all.
///
/// Both adoptions need the three states kept apart: a read failure is neither
/// an active account nor a deactivated one, and mistaking it for either is how a
/// pass that runs once leaves an account in the wrong state for good.
#[tracing::instrument(level = "debug", skip_all)]
async fn hash_empty(services: &Services, user_id: &UserId) -> Result<Option<bool>> {
	services.db["userid_password"]
		.get(user_id)
		.map_ok(|hash| hash.is_empty())
		.await
		.optional()
}
