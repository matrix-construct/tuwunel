//! Access-token expiries a foreign database keeps in a column of its own.
//!
//! That column is keyed by device, while this server keeps the expiry beside the
//! owner in the shared token column. A token the origin issued reads as never
//! expiring here until its expiry is adopted, which waits for an OIDC provider
//! the session can refresh against.

use std::{fmt, str::from_utf8, sync::Arc};

use futures::{FutureExt, Stream, TryStreamExt, future::BoxFuture};
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use tuwunel_core::{
	Result,
	result::NotFound,
	smallstr::SmallString,
	utils::{IterStream, TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::{KeyBuf, Map, SEP};

use super::{Check, Report, ScanExt, Summary, leading_record, tally};
use crate::{Services, users::device::TOKEN_LENGTH};

type AccessToken = SmallString<[u8; TOKEN_LENGTH]>;

type Column = Option<Arc<Map>>;

type Findings = (Option<Finding>, Option<Finding>);

pub(super) static CHECK: Check = Check {
	name: "foreign-token-expiry",
	about: "Where a foreign column records token expiries, no token it names stays in the \
	        foreign shape while an OIDC provider is configured, and none carries the recorded \
	        expiry while none is",
	quiescent: true,
	walk,
};

const ORIGIN_COLUMN: &str = "userdeviceid_tokenexpires";

/// A token a foreign expiry column names whose expiry is not as this server needs
/// it, or a row this check cannot read.
///
/// The adoption gives a token in the foreign shape its recorded expiry once an
/// OIDC provider is configured, and the restore takes it back while none is.
pub enum Finding {
	/// The token is in the foreign shape while a provider could refresh it.
	Unadopted {
		/// The user the origin row names.
		user_id: OwnedUserId,

		/// The device the origin row names.
		device_id: OwnedDeviceId,
	},

	/// The token is in the foreign shape, and its adoption waits for a provider.
	Waiting {
		/// The user the origin row names.
		user_id: OwnedUserId,

		/// The device the origin row names.
		device_id: OwnedDeviceId,
	},

	/// The token carries the recorded expiry while no provider could refresh it.
	Unrefreshable {
		/// The user the origin row names.
		user_id: OwnedUserId,

		/// The device the origin row names.
		device_id: OwnedDeviceId,
	},

	/// The origin row's key is not a user id and a device id.
	UnparsableDevice {
		/// The stored key.
		key: KeyBuf,
	},

	/// The origin row's value is not an expiry.
	UnparsableExpiry {
		/// The user the origin row names.
		user_id: OwnedUserId,

		/// The device the origin row names.
		device_id: OwnedDeviceId,
	},

	/// The device's stored token, or the value stored for that token, does not
	/// decode.
	UnparsableToken {
		/// The user the origin row names.
		user_id: OwnedUserId,

		/// The device the origin row names.
		device_id: OwnedDeviceId,
	},

	/// The origin row holds records past its device and expiry, which both
	/// passes ignore.
	ExtraRecords {
		/// The user the origin row names.
		user_id: OwnedUserId,

		/// The device the origin row names.
		device_id: OwnedDeviceId,
	},
}

/// One row of the origin column, decoded.
///
/// The check and both passes over the column walk the same rows, so they agree
/// on which devices the column names.
pub(crate) enum Row {
	/// The row names a device and records an expiry for its token.
	Expiry {
		/// The user the row names.
		user_id: OwnedUserId,

		/// The device the row names.
		device_id: OwnedDeviceId,

		/// The recorded expiry, in seconds since the epoch.
		expires: u64,

		/// Whether the row holds records past its device and expiry.
		extra: bool,
	},

	/// The key is not a user id and a device id.
	UnparsableDevice {
		/// The stored key.
		key: KeyBuf,
	},

	/// The value is not an expiry.
	UnparsableExpiry {
		/// The user the row names.
		user_id: OwnedUserId,

		/// The device the row names.
		device_id: OwnedDeviceId,
	},
}

/// The token an origin row's device holds here, as both passes read it.
///
/// Only a token in the foreign shape is adoptable, and only one carrying the
/// recorded expiry is restorable.
pub(crate) enum Token {
	/// The device holds no access token here.
	Absent,

	/// The device's stored token does not decode.
	Unparsable,

	/// This server does not hold the device's token.
	Unheld,

	/// The value stored for the token does not decode.
	UnparsableValue,

	/// The stored value is in the foreign shape, with no expiry field.
	Adoptable(Held),

	/// The stored value carries the recorded expiry.
	Adopted(Held),

	/// The stored value is one this server wrote, or carries another expiry.
	Kept,
}

/// A token this server holds, with the owner its stored value names.
///
/// A pass rewriting the token keeps that owner rather than the origin row's.
pub(crate) struct Held {
	/// The access token.
	pub(crate) token: AccessToken,

	/// The user the stored value names.
	pub(crate) user_id: OwnedUserId,

	/// The device the stored value names.
	pub(crate) device_id: OwnedDeviceId,
}

/// Opens the origin column, if the database holds one.
///
/// A database no foreign server has opened holds none, which is no defect.
pub fn origin(services: &Services) -> Result<Column> { services.db.open_cf(ORIGIN_COLUMN) }

/// Streams each token the origin column names whose expiry is not as this
/// server needs it.
///
/// Whether an OIDC provider is configured decides between a defect and a wait:
/// a token in the foreign shape is unadopted with one and waiting without, and
/// a token carrying the recorded expiry is reported only without one. A row
/// holding records past its shape, or one this check cannot read, is reported
/// too, while a device holding no token here and a token this server does not
/// hold are not.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan<'a>(
	services: &'a Services,
	tokenexpires: &'a Arc<Map>,
) -> impl Stream<Item = Result<Finding>> + Send + 'a {
	let refreshable = services.oauth.get_server().is_ok();

	rows(services, tokenexpires)
		.broad_and_then(move |row| findings(services, row, refreshable))
		.map_ok(|(verdict, extra)| verdict.into_iter().chain(extra).try_stream())
		.try_flatten()
}

/// Streams every row of the origin column, decoded.
///
/// A row is split by hand and read as a release build decodes it, from its
/// leading records with any more ignored, never reaching the record count a
/// debug build asserts. Nothing is buffered, so a pass consuming the rows one at
/// a time reads each token after its writes to earlier ones.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn rows<'a>(
	services: &'a Services,
	tokenexpires: &'a Arc<Map>,
) -> impl Stream<Item = Result<Row>> + Send + 'a {
	tokenexpires
		.raw_scan()
		.scanned(&services.server)
		.ready_and_then(|(key, value)| Ok(row(key, value)))
}

/// Reads the token a device holds here and classifies its stored value against
/// the recorded expiry.
///
/// Both point reads are made as this is called, so a pass calling it once per
/// row decides each row against its own earlier writes. Only a failed read is
/// an error.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn token(
	services: &Services,
	user_id: &UserId,
	device_id: &DeviceId,
	expires: u64,
) -> Result<Token> {
	let Some(token) = services.db["userdeviceid_token"]
		.qry(&(user_id, device_id))
		.await
		.optional()?
	else {
		return Ok(Token::Absent);
	};

	let Ok(token) = from_utf8(leading_record(&token).0) else {
		return Ok(Token::Unparsable);
	};

	let Some(stored) = services.db["token_userdeviceid"]
		.get(token)
		.await
		.optional()?
	else {
		return Ok(Token::Unheld);
	};

	Ok(classify(token, &stored, expires))
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Unadopted { .. } => "unadopted-expiry",
			| Self::Waiting { .. } => "waiting-for-provider",
			| Self::Unrefreshable { .. } => "unrefreshable-expiry",
			| Self::UnparsableDevice { .. } => "unparsable-device",
			| Self::UnparsableExpiry { .. } => "unparsable-expiry",
			| Self::UnparsableToken { .. } => "unparsable-token",
			| Self::ExtraRecords { .. } => "extra-records",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Unadopted { user_id, device_id } => write!(
				f,
				"token of {user_id} device {device_id} does not carry its foreign expiry"
			),
			| Self::Waiting { user_id, device_id } => write!(
				f,
				"token of {user_id} device {device_id} waits for an OIDC provider to adopt its \
				 foreign expiry"
			),
			| Self::Unrefreshable { user_id, device_id } => write!(
				f,
				"token of {user_id} device {device_id} carries its foreign expiry with no OIDC \
				 provider to refresh it"
			),
			| Self::UnparsableDevice { key } => {
				write!(f, "foreign token expiry key {} is not a device", key.escape_ascii())
			},
			| Self::UnparsableExpiry { user_id, device_id } => {
				write!(f, "foreign token expiry of {user_id} device {device_id} does not decode")
			},
			| Self::UnparsableToken { user_id, device_id } => {
				write!(f, "token of {user_id} device {device_id} does not decode")
			},
			| Self::ExtraRecords { user_id, device_id } => write!(
				f,
				"foreign token expiry of {user_id} device {device_id} holds records past its \
				 shape"
			),
		}
	}
}

// The opened column lives in this future, since the walk borrows it.
fn walk(services: &Services, limit: usize) -> BoxFuture<'_, Result<Summary>> {
	async move {
		let Some(tokenexpires) = origin(services)? else {
			return Ok(Summary::default());
		};

		tally(scan(services, &tokenexpires), limit).await
	}
	.boxed() // type-erasure boundary
}

#[tracing::instrument(level = "debug", skip_all)]
async fn findings(services: &Services, row: Row, refreshable: bool) -> Result<Findings> {
	let (user_id, device_id, expires, extra) = match row {
		| Row::Expiry { user_id, device_id, expires, extra } =>
			(user_id, device_id, expires, extra),
		| Row::UnparsableDevice { key } =>
			return Ok((Some(Finding::UnparsableDevice { key }), None)),
		| Row::UnparsableExpiry { user_id, device_id } => {
			return Ok((Some(Finding::UnparsableExpiry { user_id, device_id }), None));
		},
	};

	let token = token(services, &user_id, &device_id, expires).await?;
	let extra = extra.then(|| Finding::ExtraRecords {
		user_id: user_id.clone(),
		device_id: device_id.clone(),
	});

	let verdict = match (token, refreshable) {
		| (Token::Adoptable(_), true) => Some(Finding::Unadopted { user_id, device_id }),
		| (Token::Adoptable(_), false) => Some(Finding::Waiting { user_id, device_id }),
		| (Token::Adopted(_), false) => Some(Finding::Unrefreshable { user_id, device_id }),
		| (Token::Unparsable | Token::UnparsableValue, _) =>
			Some(Finding::UnparsableToken { user_id, device_id }),
		| (Token::Absent | Token::Unheld | Token::Kept | Token::Adopted(_), _) => None,
	};

	Ok((verdict, extra))
}

fn row(key: &[u8], value: &[u8]) -> Row {
	let Some((user_id, device_id, extra)) = device(key) else {
		return Row::UnparsableDevice { key: KeyBuf::from_slice(key) };
	};

	let (user_id, device_id) = (user_id.to_owned(), device_id.to_owned());
	let expires = value
		.get(..8)
		.and_then(|expires| expires.try_into().ok());

	match expires {
		| Some(expires) => Row::Expiry {
			user_id,
			device_id,
			expires: u64::from_be_bytes(expires),
			extra: extra || value.len() > 8,
		},
		| None => Row::UnparsableExpiry { user_id, device_id },
	}
}

/// Reads a key as a user id and a device id, as a release build decodes it.
///
/// A key with no separator reads as an empty device id, and any records past the
/// second are ignored, which the flag reports.
fn device(key: &[u8]) -> Option<(&UserId, &DeviceId, bool)> {
	let (user_id, device_id, rest) = pair(key);
	let device_id = from_utf8(device_id).ok()?;

	from_utf8(user_id)
		.ok()
		.and_then(|user_id| <&UserId>::try_from(user_id).ok())
		.map(|user_id| (user_id, device_id.into(), rest.is_some()))
}

/// Classifies a stored token value as both passes read it.
///
/// A foreign value is exactly an owner and a device, while every value this
/// server writes has an expiry field, so a value with none is adoptable
/// whatever its expiry would be. A value is restorable only when it carries
/// exactly the recorded expiry, the adoption's own write.
pub(super) fn classify(token: &str, stored: &[u8], expires: u64) -> Token {
	let Some((user_id, device_id, carried)) = token_value(stored) else {
		return Token::UnparsableValue;
	};

	let held = || Held {
		token: token.into(),
		user_id: user_id.to_owned(),
		device_id: device_id.to_owned(),
	};

	let fields = user_id
		.as_str()
		.len()
		.saturating_add(1)
		.saturating_add(device_id.as_str().len());

	let foreign = carried.is_none() && stored.len() == fields;

	match carried {
		| None if foreign => Token::Adoptable(held()),
		| Some(carried) if carried == expires => Token::Adopted(held()),
		| None | Some(_) => Token::Kept,
	}
}

/// Reads a stored token value as a release build decodes it.
///
/// A value with no separator reads as an empty device id, an expiry field that
/// is empty or begins with a separator reads as none, and anything past the
/// eight bytes of an expiry is ignored. The expiry's own bytes can hold a
/// separator, so only the first two separators split.
fn token_value(stored: &[u8]) -> Option<(&UserId, &DeviceId, Option<u64>)> {
	let (user_id, device_id, expiry) = pair(stored);
	let device_id = from_utf8(device_id).ok()?;
	let carried = match expiry {
		| None | Some([] | [SEP, ..]) => None,
		| Some(expiry) => Some(u64::from_be_bytes(expiry.get(..8)?.try_into().ok()?)),
	};

	from_utf8(user_id)
		.ok()
		.and_then(|user_id| <&UserId>::try_from(user_id).ok())
		.map(|user_id| (user_id, device_id.into(), carried))
}

/// Splits a key or value into its first two records and whatever follows them,
/// as a release build reads a pair.
///
/// With no separator the second record is empty, and the rest is `None` unless
/// a separator ends the second record.
fn pair(bytes: &[u8]) -> (&[u8], &[u8], Option<&[u8]>) {
	let (first, rest) = leading_record(bytes);
	let (second, rest) = rest.map(leading_record).unwrap_or_default();

	(first, second, rest)
}
