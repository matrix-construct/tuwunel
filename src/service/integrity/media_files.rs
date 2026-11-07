//! The files of `mediaid_file` rows in the media directory.
//!
//! A row's file is named by the base64 of its key's SHA-256 digest, or under the
//! legacy naming by the base64 of the key itself. One check covers the rename
//! from legacy names, the other the directory once that rename is stamped.

use std::{
	collections::HashSet,
	ffi::OsString,
	fmt,
	fs::{Metadata, read_dir},
	io::{self, ErrorKind},
	path::{Path, PathBuf},
};

use futures::{Stream, TryStreamExt, stream::once};
use tokio::fs::symlink_metadata;
use tuwunel_core::{
	Err, Result, err,
	itertools::Itertools,
	utils::{BoolExt, TryReadyExt, stream::TryBroadbandExt},
};
use tuwunel_database::KeyBuf;

use super::{Check, Report, ScanExt, tally};
use crate::{
	Services,
	migrations::{SHA256_MEDIA, marker_present},
};

type Files = HashSet<OsString>;

pub(super) static LEGACY: Check = Check {
	name: "legacy-media-names",
	about: "Reports only while the sha256 media rename is unstamped: no mediaid_file row's \
	        media is still a file under its legacy name",
	quiescent: false,
	walk: |services, limit| tally(legacy(services), limit),
};

pub(super) static CHECK: Check = Check {
	name: "media-files",
	about: "Reports only once the sha256 media rename is stamped: every mediaid_file row has an \
	        entry in the media directory under its sha256 or legacy name; with compat_file_link \
	        it has both, and without it the legacy name is no link",
	quiescent: true,
	walk: |services, limit| tally(scan(services), limit),
};

/// A `mediaid_file` row whose media is still a file under its legacy name.
///
/// The rename moves each such file to its sha256 name, and with
/// `compat_file_link` links the legacy name to it. A link already there is the
/// rename's own work, so it is not reported.
pub struct Legacy {
	/// The stored key.
	pub key: KeyBuf,
}

/// A `mediaid_file` row the media directory disagrees with.
///
/// Each kind is a case the startup media check acts on. It deletes a row missing
/// both names only with `prune_missing`, while this check reports one either way.
pub enum Finding {
	/// Neither name is in the directory.
	Missing {
		/// The stored key.
		key: KeyBuf,
	},

	/// With `compat_file_link`, the sha256 name is in the directory but the
	/// legacy name is not.
	Unlinked {
		/// The stored key.
		key: KeyBuf,
	},

	/// With `compat_file_link`, the legacy name is in the directory but the
	/// sha256 name is not.
	Unmigrated {
		/// The stored key.
		key: KeyBuf,
	},

	/// Without `compat_file_link`, the legacy name is a link.
	Linked {
		/// The stored key.
		key: KeyBuf,
	},
}

/// A row's legacy name, and which of its two names the directory lists.
///
/// Only the legacy name's kind is ever read from the filesystem past the listing,
/// so it is the one name kept.
struct Entry {
	key: KeyBuf,
	legacy: PathBuf,
	legacy_listed: bool,
	sha256_listed: bool,
}

/// Streams each `mediaid_file` row whose media is still a file under its legacy
/// name, only while the sha256 rename is unstamped.
///
/// The legacy name is read without following a link, so the link the rename
/// leaves with `compat_file_link` is not reported. Once the rename is stamped
/// the startup media check owns the directory, and [`scan`] reports it.
#[tracing::instrument(level = "trace", skip_all)]
pub fn legacy(services: &Services) -> impl Stream<Item = Result<Legacy>> + Send + '_ {
	once(marker_present(services, SHA256_MEDIA))
		.map_ok(move |renamed| renamed.is_false().then(|| keys(services)))
		.ready_try_filter_map(Ok)
		.try_flatten()
		.broad_and_then(async |key| unrenamed(services, key).await)
		.ready_try_filter_map(Ok)
}

/// Streams each `mediaid_file` row the media directory disagrees with, only
/// once the sha256 rename is stamped.
///
/// Every row is judged against one listing of the directory, under the four
/// cases the startup media check acts on. A directory that does not list, or a
/// legacy name whose kind cannot be read, is an error rather than a finding.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	let compat = services.server.config.media.compat_file_link;

	once(files(services))
		.ready_try_filter_map(Ok)
		.map_ok(move |files| entries(services, files))
		.try_flatten()
		.broad_and_then(move |entry| classify(entry, compat))
		.ready_try_filter_map(Ok)
}

/// Lists the media directory, naming each entry by `dir` joined with its name.
///
/// The startup media check and [`scan`] test a row's two names against this
/// listing, so both read the directory alike. The startup check drops whatever
/// fails to read, where [`scan`] fails on it.
pub(crate) fn listing(dir: &Path) -> io::Result<impl Iterator<Item = io::Result<OsString>>> {
	read_dir(dir).map(|entries| entries.map_ok(|entry| entry.path().into_os_string()))
}

impl Report for Legacy {
	fn kind(&self) -> &'static str { "unrenamed-file" }
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Missing { .. } => "missing-file",
			| Self::Unlinked { .. } => "missing-link",
			| Self::Unmigrated { .. } => "unmigrated-file",
			| Self::Linked { .. } => "stale-link",
		}
	}
}

impl fmt::Display for Legacy {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "media {} is still a file under its legacy name", self.key.escape_ascii())
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Missing { key } => {
				write!(
					f,
					"media {} has neither its sha256 nor its legacy name",
					key.escape_ascii()
				)
			},
			| Self::Unlinked { key } => {
				write!(f, "media {} has no link under its legacy name", key.escape_ascii())
			},
			| Self::Unmigrated { key } => {
				write!(f, "media {} has its legacy name but no sha256 name", key.escape_ascii())
			},
			| Self::Linked { key } => write!(
				f,
				"media {} keeps a legacy link with compat_file_link off",
				key.escape_ascii()
			),
		}
	}
}

fn keys(services: &Services) -> impl Stream<Item = Result<KeyBuf>> + Send + '_ {
	services.db["mediaid_file"]
		.raw_scan()
		.scanned(&services.server)
		.map_ok(|(key, _)| KeyBuf::from_slice(key))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn unrenamed(services: &Services, key: KeyBuf) -> Result<Option<Legacy>> {
	let path = services.media.get_media_path_b64(&key);

	symlink_metadata(&path)
		.await
		.map(|meta| {
			meta.is_symlink()
				.is_false()
				.then_some(Legacy { key })
		})
		.or_else(|error| match error.kind() {
			| ErrorKind::NotFound => Ok(None),
			| _ => Err!("cannot read {}: {error}", path.display()),
		})
}

/// Lists the media directory once the sha256 rename is stamped.
///
/// Before then the rename owns the legacy names, so there is nothing to list.
/// Any entry that fails to read fails the whole listing.
#[tracing::instrument(level = "debug", skip_all)]
async fn files(services: &Services) -> Result<Option<Files>> {
	marker_present(services, SHA256_MEDIA)
		.await?
		.then(|| {
			let dir = services.media.get_media_dir();

			listing(&dir)
				.and_then(Iterator::collect)
				.map_err(|error| {
					err!("cannot list the media directory {}: {error}", dir.display())
				})
		})
		.transpose()
}

fn entries(services: &Services, files: Files) -> impl Stream<Item = Result<Entry>> + Send + '_ {
	keys(services).map_ok(move |key| entry(services, &files, key))
}

fn entry(services: &Services, files: &Files, key: KeyBuf) -> Entry {
	let sha256 = services
		.media
		.get_media_path_sha256(&key)
		.into_os_string();

	let legacy = services.media.get_media_path_b64(&key);

	Entry {
		legacy_listed: files.contains(legacy.as_os_str()),
		sha256_listed: files.contains(&sha256),
		legacy,
		key,
	}
}

/// Judges one row as the startup media check does, reading the legacy name's
/// kind only where the check would.
///
/// A row missing both names is reported whatever `prune_missing` says, since
/// that setting decides only whether the startup check deletes it.
#[tracing::instrument(level = "debug", skip_all)]
async fn classify(entry: Entry, compat: bool) -> Result<Option<Finding>> {
	let Entry {
		key,
		legacy,
		legacy_listed,
		sha256_listed,
	} = entry;

	let finding = match (legacy_listed, sha256_listed) {
		| (false, false) => Some(Finding::Missing { key }),
		| (false, true) if compat => Some(Finding::Unlinked { key }),
		| (true, false) if compat => Some(Finding::Unmigrated { key }),
		| (true, _) if !compat => is_link(&legacy)
			.await?
			.then_some(Finding::Linked { key }),
		| _ => None,
	};

	Ok(finding)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn is_link(path: &Path) -> Result<bool> {
	symlink_metadata(path)
		.await
		.as_ref()
		.map(Metadata::is_symlink)
		.map_err(|error| err!("cannot read {}: {error}", path.display()))
}
