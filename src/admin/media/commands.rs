use std::time::Duration;

use ruma::{CanonicalJsonValue, Mxc, OwnedEventId, OwnedMxcUri, OwnedServerName};
use tuwunel_core::{
	Err, Result, debug, err, error, info, trace,
	utils::{math::Expected, time::parse_timepoint_ago},
	warn,
};
use tuwunel_service::media::Dim;

use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn delete(&self, mxc: OwnedMxcUri) -> Result {
	self.services
		.media
		.delete(&mxc.as_str().try_into()?)
		.await?;

	Err!("Deleted the MXC from our database and on our filesystem.")
}

#[admin_command]
pub(super) async fn delete_by_event(&self, event_id: OwnedEventId) -> Result {
	let mut mxc_urls = Vec::with_capacity(3);

	// parsing the PDU for any MXC URLs begins here
	let event_json = self
		.services
		.timeline
		.get_pdu_json(&event_id)
		.await
		.map_err(|_| err!("Event ID does not exist or is not known to us."))?;

	let content = event_json
		.get("content")
		.and_then(CanonicalJsonValue::as_object)
		.ok_or_else(|| {
			err!(
				"Event ID does not have a \"content\" key, this is not a message or an event \
				 type that contains media.",
			)
		})?;

	// 1. attempts to parse the "url" key
	debug!("Attempting to go into \"url\" key for main media file");
	if let Some(url) = content
		.get("url")
		.and_then(CanonicalJsonValue::as_str)
	{
		debug!("Got a URL in the event ID {event_id}: {url}");

		mxc_urls.push(url.to_owned());
	} else {
		debug!("No main media found.");
	}

	// 2. attempts to parse the "info" key
	debug!("Attempting to go into \"info\" key for thumbnails");
	if let Some(thumbnail_url) = content
		.get("info")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|info| info.get("thumbnail_url"))
		.and_then(CanonicalJsonValue::as_str)
	{
		debug!("Found a thumbnail_url in info key: {thumbnail_url}");

		mxc_urls.push(thumbnail_url.to_owned());
	} else {
		debug!("No thumbnails found.");
	}

	// 3. attempts to parse the "file" key
	debug!("Attempting to go into \"file\" key");
	if let Some(url) = content
		.get("file")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|file| file.get("url"))
		.and_then(CanonicalJsonValue::as_str)
	{
		debug!("Found url in file key: {url}");

		mxc_urls.push(url.to_owned());
	} else {
		debug!("No \"url\" key in \"file\" key.");
	}

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

	self.write_str(&format!(
		"Deleted {mxc_deletion_count} total MXCs from our database and the filesystem from \
		 event ID {event_id}."
	))
	.await
}

#[admin_command]
pub(super) async fn delete_list(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	let mut failed_parsed_mxcs: usize = 0;

	let mxc_list = self
		.body
		.to_vec()
		.drain(1..self.body.len().expected_sub(1))
		.filter_map(|mxc_s| {
			mxc_s
				.try_into()
				.inspect_err(|e| {
					warn!("Failed to parse user-provided MXC URI: {e}");
					failed_parsed_mxcs = failed_parsed_mxcs.saturating_add(1);
				})
				.ok()
		})
		.collect::<Vec<Mxc<'_>>>();

	let mut mxc_deletion_count: usize = 0;

	for mxc in &mxc_list {
		trace!(%failed_parsed_mxcs, %mxc_deletion_count, "Deleting MXC {mxc} in bulk");
		match self.services.media.delete(mxc).await {
			| Ok(()) => {
				info!("Successfully deleted {mxc} from filesystem and database");
				mxc_deletion_count = mxc_deletion_count.saturating_add(1);
			},
			| Err(e) => {
				warn!("Failed to delete {mxc}, ignoring error and skipping: {e}");
				continue;
			},
		}
	}

	self.write_str(&format!(
		"Finished bulk MXC deletion, deleted {mxc_deletion_count} total MXCs from our database \
		 and the filesystem. {failed_parsed_mxcs} MXCs failed to be parsed from the database.",
	))
	.await
}

#[admin_command]
pub(super) async fn delete_past_remote_media(
	&self,
	duration: String,
	before: bool,
	after: bool,
	yes_i_want_to_delete_local_media: bool,
) -> Result {
	if before && after {
		return Err!("Please only pick one argument, --before or --after.",);
	}

	let duration = parse_timepoint_ago(&duration)?;
	let deleted_count = self
		.services
		.media
		.delete_all_remote_media_at_after_time(
			duration,
			before,
			after,
			yes_i_want_to_delete_local_media,
		)
		.await?;

	self.write_str(&format!("Deleted {deleted_count} total files.",))
		.await
}

#[admin_command]
pub(super) async fn delete_all_from_user(&self, username: String) -> Result {
	let user_id = parse_local_user_id(self.services, &username)?;

	let deleted_count = self
		.services
		.media
		.delete_from_user(&user_id)
		.await?;

	self.write_str(&format!("Deleted {deleted_count} total files.",))
		.await
}

#[admin_command]
pub(super) async fn delete_all_from_server(
	&self,
	server_name: OwnedServerName,
	yes_i_want_to_delete_local_media: bool,
) -> Result {
	if server_name == self.services.globals.server_name() && !yes_i_want_to_delete_local_media {
		return Err!("This command only works for remote media by default.",);
	}

	let Ok(all_mxcs) = self
		.services
		.media
		.get_all_mxcs()
		.await
		.inspect_err(|e| error!("Failed to get MXC URIs from our database: {e}"))
	else {
		return Err!("Failed to get MXC URIs from our database",);
	};

	let mut deleted_count: usize = 0;

	for mxc in all_mxcs {
		let Ok(mxc_server_name) = mxc.server_name().inspect_err(|e| {
			warn!(
				"Failed to parse MXC {mxc} server name from database, ignoring error and \
				 skipping: {e}"
			);
		}) else {
			continue;
		};

		if mxc_server_name != server_name {
			trace!("skipping MXC URI {mxc}");
			continue;
		}

		let mxc: Mxc<'_> = mxc.as_str().try_into()?;

		match self.services.media.delete(&mxc).await {
			| Ok(()) => {
				deleted_count = deleted_count.saturating_add(1);
			},
			| Err(e) => {
				warn!("Failed to delete {mxc}, ignoring error and skipping: {e}");
			},
		}
	}

	self.write_str(&format!("Deleted {deleted_count} total files.",))
		.await
}

#[admin_command]
pub(super) async fn get_file_info(&self, mxc: OwnedMxcUri) -> Result {
	let mxc: Mxc<'_> = mxc.as_str().try_into()?;
	let metadata = self.services.media.get_metadata(&mxc).await;

	self.write_str(&format!("```\n{metadata:#?}\n```"))
		.await
}

#[admin_command]
pub(super) async fn get_remote_file(
	&self,
	mxc: OwnedMxcUri,
	server: Option<OwnedServerName>,
	timeout: u32,
) -> Result {
	let mxc: Mxc<'_> = mxc.as_str().try_into()?;
	let timeout = Duration::from_millis(timeout.into());
	let mut result = self
		.services
		.media
		.fetch_remote_content(&mxc, None, server.as_deref(), timeout)
		.await?;

	// Grab the length of the content before clearing it to not flood the output
	let len = result.content.as_ref().expect("content").len();
	result.content.as_mut().expect("content").clear();

	self.write_str(&format!("```\n{result:#?}\nreceived {len} bytes for file content.\n```"))
		.await
}

#[admin_command]
pub(super) async fn get_remote_thumbnail(
	&self,
	mxc: OwnedMxcUri,
	server: Option<OwnedServerName>,
	timeout: u32,
	width: u32,
	height: u32,
) -> Result {
	let mxc: Mxc<'_> = mxc.as_str().try_into()?;
	let timeout = Duration::from_millis(timeout.into());
	let dim = Dim::new(width, height, None);
	let mut result = self
		.services
		.media
		.fetch_remote_thumbnail(&mxc, None, server.as_deref(), timeout, &dim)
		.await?;

	// Grab the length of the content before clearing it to not flood the output
	let len = result.content.as_ref().expect("content").len();
	result.content.as_mut().expect("content").clear();

	self.write_str(&format!("```\n{result:#?}\nreceived {len} bytes for file content.\n```"))
		.await
}
