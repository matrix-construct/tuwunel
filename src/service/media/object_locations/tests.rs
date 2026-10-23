use ruma::{Mxc, server_name, user_id};
use tuwunel_core::{Result, config::Figment};

use super::{
	super::{Dim, Service},
	MediaObjectStatus,
};
use crate::test_utils::fixture;

#[tokio::test]
async fn object_locations_distinguish_invalid_metadata_from_absent_media() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};
	let media = &fixture.services.media;
	let broken = Mxc {
		server_name: server_name!("localhost"),
		media_id: "broken-metadata",
	};
	let valid = Mxc {
		server_name: server_name!("localhost"),
		media_id: "valid-metadata",
	};
	let mut key = media.db.create_file_metadata(
		&broken,
		None,
		&Dim::default(),
		None,
		Some("text/plain"),
	)?;
	fixture.services.db["mediaid_file"].remove(&key);
	*key.last_mut()
		.expect("metadata key contains a content type") = 0xFE;
	fixture.services.db["mediaid_file"].insert(&key, []);
	media
		.db
		.create_file_metadata(&valid, None, &Dim::default(), None, Some("text/plain"))?;

	let results = media
		.object_locations(
			&[broken.to_string(), "mxc://localhost/missing".to_owned(), valid.to_string()],
			&["media".to_owned()],
			true,
		)
		.await?;
	assert_eq!(
		results[0].status,
		MediaObjectStatus::MetadataUnavailable,
		"a decoding failure must not report absence"
	);
	assert!(results[0].objects.is_empty(), "unreadable metadata cannot supply a locator");
	assert_eq!(
		results[1].status,
		MediaObjectStatus::NotFound,
		"missing metadata must still report absence"
	);
	assert_eq!(
		results[2].status,
		MediaObjectStatus::UnsupportedBackend,
		"one corrupt item must not fail the batch"
	);
	Ok(())
}

#[tokio::test]
async fn object_locations_only_resolve_original_metadata() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};
	let media: &Service = &fixture.services.media;
	let thumbnail = Mxc {
		server_name: server_name!("localhost"),
		media_id: "thumbnail-only",
	};
	let pending = Mxc {
		server_name: server_name!("localhost"),
		media_id: "pending-only",
	};
	media.db.create_file_metadata(
		&thumbnail,
		None,
		&Dim::new(32, 32, None),
		None,
		Some("image/png"),
	)?;
	media
		.db
		.insert_pending_mxc(&pending, user_id!("@alice:localhost"), u64::MAX);

	let uris = [
		thumbnail.to_string(),
		pending.to_string(),
		"mxc://never-fetch.invalid/absent".to_owned(),
	];
	let providers = media.object_location_providers(None)?;
	let results = media
		.object_locations(&uris, &providers, true)
		.await?;
	assert_eq!(results.len(), 3);
	assert!(
		results
			.iter()
			.all(|result| result.status == MediaObjectStatus::NotFound
				&& result.objects.is_empty())
	);
	assert!(media.get_metadata(&thumbnail).await.is_none());
	assert!(
		media
			.db
			.search_pending_mxc(&pending)
			.await
			.is_ok()
	);
	Ok(())
}

#[cfg(feature = "url_preview")]
#[tokio::test]
async fn object_locations_leave_lazy_media_unmaterialized() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};
	let media = &fixture.services.media;
	let lazy = Mxc {
		server_name: server_name!("localhost"),
		media_id: "lazy-only",
	};
	let url = "https://never-fetch.invalid/lazy";
	media.db.insert_lazy_media(&lazy.to_string(), url);
	let results = media
		.object_locations(&[lazy.to_string()], &["media".to_owned()], true)
		.await?;
	assert_eq!(results[0].status, MediaObjectStatus::NotFound);
	assert!(media.get_metadata(&lazy).await.is_none());
	assert_eq!(media.db.search_lazy_media(&lazy).await?, url);
	Ok(())
}
