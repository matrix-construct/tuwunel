//! Media rows against the files the media directory holds.
//!
//! The fixture keeps its media directory under its own database path, so each
//! test writes files there beside the rows naming them.

use std::{
	fs::{create_dir_all, read, read_link, remove_dir, remove_dir_all, symlink_metadata, write},
	io::{self, ErrorKind},
	os::unix::fs::symlink,
	path::Path,
};

use tuwunel_core::{Result, config::Figment};

use super::{count, present, walk};
use crate::{
	Services,
	media::migrations::checkup_sha256_media,
	migrations::{SHA256_MEDIA, fresh, marker_present, migrate},
	test_utils::fixture,
};

const MISSING: &[u8] = b"mxc://localhost/missing\xFF";

const UNLINKED: &[u8] = b"mxc://localhost/unlinked\xFF";

const UNMIGRATED: &[u8] = b"mxc://localhost/unmigrated\xFF";

const LINKED: &[u8] = b"mxc://localhost/linked\xFF";

const LEGACY: &[u8] = b"mxc://localhost/legacy\xFF";

const BOTH: &[u8] = b"mxc://localhost/both\xFF";

const BLOCKED: &[u8] = b"mxc://localhost/blocked\xFF";

const UPLOADER: &[u8] = b"mxc://localhost/missing\xFF@alice:localhost";

const CONTENT: &[u8] = b"media";

#[tokio::test]
async fn files_check_reports_each_linked_case() -> Result {
	let config = Figment::new()
		.merge(("admin.room.create", false))
		.merge(("media.compat_file_link", true));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let rows = &services.db["mediaid_file"];

	fresh(services).await?;
	for key in [MISSING, UNLINKED, UNMIGRATED, BOTH] {
		rows.insert(key, []);
	}

	sha256_file(services, UNLINKED)?;
	legacy_file(services, UNMIGRATED)?;
	sha256_file(services, BOTH)?;
	legacy_link(services, BOTH)?;

	let summary = walk(services, "media-files").await?;

	assert_eq!(summary.counts.get("missing-file"), Some(&1));
	assert_eq!(summary.counts.get("missing-link"), Some(&1));
	assert_eq!(summary.counts.get("unmigrated-file"), Some(&1));
	assert_eq!(summary.findings(), 3);
	assert_eq!(count(services, "legacy-media-names").await?, 0);

	Ok(())
}

fn sha256_file(services: &Services, key: &[u8]) -> io::Result<()> {
	write(services.media.get_media_path_sha256(key), CONTENT)
}

fn legacy_file(services: &Services, key: &[u8]) -> io::Result<()> {
	write(services.media.get_media_path_b64(key), CONTENT)
}

fn legacy_link(services: &Services, key: &[u8]) -> io::Result<()> {
	let media = &services.media;

	symlink(media.get_media_path_sha256(key), media.get_media_path_b64(key))
}

#[tokio::test]
async fn startup_check_removes_only_a_stale_link() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let rows = &services.db["mediaid_file"];
	let legacy = |key| services.media.get_media_path_b64(key);

	fresh(services).await?;
	for key in [MISSING, LINKED, LEGACY, BOTH] {
		rows.insert(key, []);
	}

	sha256_file(services, LINKED)?;
	legacy_link(services, LINKED)?;
	legacy_file(services, LEGACY)?;
	sha256_file(services, BOTH)?;
	legacy_file(services, BOTH)?;

	let summary = walk(services, "media-files").await?;

	assert_eq!(summary.counts.get("stale-link"), Some(&1));
	assert_eq!(summary.counts.get("missing-file"), Some(&1));
	assert_eq!(summary.findings(), 2);

	checkup_sha256_media(services).await?;

	assert!(absent(legacy(LINKED)));
	assert_eq!(read(services.media.get_media_path_sha256(LINKED))?, CONTENT);
	assert_eq!(read(legacy(LEGACY))?, CONTENT);
	assert_eq!(read(legacy(BOTH))?, CONTENT);
	assert!(present(rows, MISSING).await);

	let summary = walk(services, "media-files").await?;

	assert_eq!(summary.counts.get("missing-file"), Some(&1));
	assert_eq!(summary.findings(), 1);

	Ok(())
}

fn absent(path: impl AsRef<Path>) -> bool {
	symlink_metadata(path).is_err_and(|error| error.kind().eq(&ErrorKind::NotFound))
}

#[tokio::test]
async fn startup_check_prunes_and_links() -> Result {
	let config = Figment::new()
		.merge(("admin.room.create", false))
		.merge(("media.compat_file_link", true))
		.merge(("media.prune_missing", true));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let (rows, uploaders) = (&services.db["mediaid_file"], &services.db["mediaid_user"]);
	let media = &services.media;

	fresh(services).await?;
	rows.insert(MISSING, []);
	rows.insert(UNLINKED, []);
	uploaders.insert(UPLOADER, "@alice:localhost");
	sha256_file(services, UNLINKED)?;

	assert_eq!(walk(services, "media-files").await?.findings(), 2);

	checkup_sha256_media(services).await?;

	assert!(!present(rows, MISSING).await);
	assert!(present(uploaders, UPLOADER).await);
	assert_eq!(
		read_link(media.get_media_path_b64(UNLINKED))?,
		media.get_media_path_sha256(UNLINKED)
	);

	assert_eq!(walk(services, "media-files").await?.findings(), 0);

	Ok(())
}

#[tokio::test]
async fn files_check_fails_on_an_unreadable_directory() -> Result {
	let config = Figment::new()
		.merge(("admin.room.create", false))
		.merge(("media.prune_missing", true));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let rows = &services.db["mediaid_file"];

	fresh(services).await?;
	rows.insert(MISSING, []);
	remove_dir_all(services.media.get_media_dir())?;

	assert!(walk(services, "media-files").await.is_err());

	// The startup check reads a directory that does not list as an empty one.
	checkup_sha256_media(services).await?;

	assert!(!present(rows, MISSING).await);

	Ok(())
}

#[tokio::test]
async fn legacy_names_check_reports_an_unrenamed_file() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let rows = &services.db["mediaid_file"];

	fresh(services).await?;
	services.db["global"].remove(SHA256_MEDIA);
	for key in [MISSING, LINKED, LEGACY] {
		rows.insert(key, []);
	}

	sha256_file(services, LINKED)?;
	legacy_link(services, LINKED)?;
	legacy_file(services, LEGACY)?;

	let summary = walk(services, "legacy-media-names").await?;

	assert_eq!(summary.counts.get("unrenamed-file"), Some(&1));
	assert_eq!(summary.findings(), 1);
	assert_eq!(walk(services, "media-files").await?.findings(), 0);

	services.db["global"].insert(SHA256_MEDIA, []);

	assert_eq!(count(services, "legacy-media-names").await?, 0);

	Ok(())
}

#[tokio::test]
async fn rename_stamps_only_when_it_succeeds() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let rows = &services.db["mediaid_file"];
	let media = &services.media;

	fresh(services).await?;
	services.db["global"].remove(SHA256_MEDIA);
	rows.insert(BLOCKED, []);
	rows.insert(LEGACY, []);
	legacy_file(services, BLOCKED)?;
	legacy_file(services, LEGACY)?;

	// A directory under the sha256 name refuses the rename onto it.
	create_dir_all(media.get_media_path_sha256(BLOCKED))?;

	assert!(migrate(services, false).await.is_err());
	assert!(!marker_present(services, SHA256_MEDIA).await?);
	assert_eq!(count(services, "legacy-media-names").await?, 2);

	remove_dir(media.get_media_path_sha256(BLOCKED))?;
	migrate(services, false).await?;

	assert!(marker_present(services, SHA256_MEDIA).await?);
	assert_eq!(read(media.get_media_path_sha256(BLOCKED))?, CONTENT);
	assert_eq!(read(media.get_media_path_sha256(LEGACY))?, CONTENT);
	assert!(absent(media.get_media_path_b64(LEGACY)));
	assert_eq!(walk(services, "media-files").await?.findings(), 0);

	Ok(())
}
