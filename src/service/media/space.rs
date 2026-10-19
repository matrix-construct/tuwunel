use std::{path::Path, sync::Arc};

use http::StatusCode;
use ruma::api::error::{ErrorKind, ResourceLimitExceededErrorData};
use tuwunel_core::{
	Error, Result,
	config::StorageProvider,
	defer, extract_variant, implement,
	smallvec::SmallVec,
	utils::{result::LogDebugErr, sys::storage::available_space},
};

use crate::storage::Provider;

// an upload is nearly always written to one provider
type Providers = SmallVec<[Arc<Provider>; 1]>;

/// Refuse an upload while storage it would be written to is short of space.
///
/// Each local storage provider an upload is written to is asked for the space
/// available on its filesystem, and the upload is refused while any of them
/// has less than `media_upload_min_free_space`. A provider that is not a
/// directory has no such figure, and one that cannot be read refuses nothing.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn check_upload_space(&self) -> Result {
	let min_free_space = self.services.config.media_upload_min_free_space;

	if min_free_space == 0 {
		return Ok(());
	}

	let providers: Providers = self
		.upload_providers()
		.filter(|provider| matches!(provider.config, StorageProvider::local(_)))
		.cloned()
		.collect();

	if providers.is_empty() {
		return Ok(());
	}

	let task = self
		.services
		.server
		.runtime()
		.spawn_blocking(move || any_short_of_space(&providers, min_free_space));

	// on cancellation, drop a read still queued for the blocking pool
	let abort = task.abort_handle();

	defer! {{ abort.abort(); }}

	if !task.await? {
		return Ok(());
	}

	Err(Error::Request(
		// the spec requires admin_contact; clients use support discovery
		ErrorKind::ResourceLimitExceeded(ResourceLimitExceededErrorData::new(String::new())),
		"The server is short of storage space and is not accepting uploads.".into(),
		StatusCode::FORBIDDEN,
	))
}

/// Whether any local provider's filesystem is short of space.
///
/// A filesystem that cannot be read is passed over.
fn any_short_of_space<'a, I>(providers: I, min_free_space: u64) -> bool
where
	I: IntoIterator<Item = &'a Arc<Provider>>,
{
	providers
		.into_iter()
		.filter_map(|provider| extract_variant!(&provider.config, StorageProvider::local))
		.filter_map(|local| {
			available_space(Path::new(&local.base_path))
				.log_debug_err()
				.ok()
		})
		.any(|available| is_short_of_space(available, min_free_space))
}

/// Whether this much available space refuses an upload.
///
/// Space exactly at the minimum is enough, so only less than it refuses.
#[inline]
#[must_use]
pub(super) fn is_short_of_space(available: u64, min_free_space: u64) -> bool {
	available < min_free_space
}
