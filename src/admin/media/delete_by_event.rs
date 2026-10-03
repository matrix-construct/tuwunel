use ruma::{CanonicalJsonObject, CanonicalJsonValue, Mxc, OwnedEventId};
use tuwunel_core::{
	Err, Result, debug, err, info,
	utils::{BoolExt, to_canonical_object},
	warn,
};
use tuwunel_matrix::Event;

use crate::admin_command;

#[admin_command]
pub(super) async fn delete_by_event(&self, event_id: OwnedEventId) -> Result {
	let event = self
		.services
		.timeline
		.get_pdu(&event_id)
		.await
		.map_err(|error| match error.is_not_found() {
			| true => err!("Event ID does not exist or is not known to us."),
			| false => error,
		})?;

	let event_json = event
		.is_redacted()
		.then_async(async || {
			self.services
				.retention
				.get_original_pdu_json(&event_id)
				.await
				.map_err(|error| match error.is_not_found() {
					| true => err!("Redacted event original unavailable."),
					| false => error,
				})
		})
		.await
		.unwrap_or_else(|| to_canonical_object(event).map_err(Into::into))?;

	let content = event_json
		.get("content")
		.and_then(CanonicalJsonValue::as_object)
		.ok_or_else(|| {
			err!(
				"Event ID does not have a \"content\" key, this is not a message or an event \
				 type that contains media.",
			)
		})?;

	let mut mxc_urls = Vec::with_capacity(3);
	mxc_urls.extend(url_mxc_from_content(content));
	mxc_urls.extend(thumbnail_mxc_from_content(content));
	mxc_urls.extend(file_mxc_from_content(content));

	if mxc_urls.is_empty() {
		return Err!("Parsed event ID but found no MXC URLs.",);
	}

	let mut mxc_deletion_count: usize = 0;

	for mxc_url in mxc_urls {
		if !mxc_url.starts_with("mxc://") {
			warn!("Ignoring non-mxc url {mxc_url}");
			continue;
		}

		let mxc: Mxc<'_> = mxc_url.as_str().try_into()?;

		match self.services.media.delete(&mxc).await {
			| Ok(()) => {
				info!("Successfully deleted {mxc_url} from filesystem and database");
				mxc_deletion_count = mxc_deletion_count.saturating_add(1);
			},
			| Err(e) => {
				warn!("Failed to delete {mxc_url}, ignoring error and skipping: {e}");
			},
		}
	}

	write!(
		self,
		"Deleted {mxc_deletion_count} total MXCs from our database and the filesystem from \
		 event ID {event_id}."
	)
	.await
}

fn url_mxc_from_content(content: &CanonicalJsonObject) -> Option<String> {
	debug!("Attempting to go into \"url\" key for main media file");
	let url = content
		.get("url")
		.and_then(CanonicalJsonValue::as_str)?;

	debug!("Got main media URL: {url}");
	Some(url.to_owned())
}

fn thumbnail_mxc_from_content(content: &CanonicalJsonObject) -> Option<String> {
	debug!("Attempting to go into \"info\" key for thumbnails");
	let thumbnail_url = content
		.get("info")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|info| info.get("thumbnail_url"))
		.and_then(CanonicalJsonValue::as_str)?;

	debug!("Found a thumbnail_url in info key: {thumbnail_url}");
	Some(thumbnail_url.to_owned())
}

fn file_mxc_from_content(content: &CanonicalJsonObject) -> Option<String> {
	debug!("Attempting to go into \"file\" key");
	let url = content
		.get("file")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|file| file.get("url"))
		.and_then(CanonicalJsonValue::as_str)?;

	debug!("Found url in file key: {url}");
	Some(url.to_owned())
}
