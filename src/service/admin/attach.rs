use ruma::{
	Mxc,
	events::room::{
		MediaSource,
		message::{FileInfo, FileMessageEventContent, MessageType, RoomMessageEventContent},
	},
};
use tuwunel_core::{
	Result, implement,
	utils::{
		content_disposition::make_content_disposition, math::ruma_from_usize, random_string,
		time::now_secs,
	},
};

use super::CommandOutput;
use crate::media::MXC_LENGTH;

/// Uploads a command's output to the media repository and builds an m.file
/// message referencing it. The caller attaches the relation.
#[implement(super::Service)]
pub(super) async fn attach(&self, output: &CommandOutput) -> Result<RoomMessageEventContent> {
	let (mime, ext) = match output {
		| CommandOutput::Markdown(_) => ("text/markdown", "md"),
		| CommandOutput::Plain(_) => ("text/plain", "txt"),
	};

	let text = output.as_str();
	let filename = format!("admin-output-{}.{ext}", now_secs());
	let media_id = random_string(MXC_LENGTH);
	let mxc = Mxc {
		server_name: self.services.globals.server_name(),
		media_id: &media_id,
	};

	let content_disposition = make_content_disposition(None, Some(mime), Some(&filename));

	self.services
		.media
		.create(
			&mxc,
			Some(&self.services.globals.server_user),
			Some(&content_disposition),
			Some(mime),
			text.as_bytes(),
		)
		.await?;

	let file = FileMessageEventContent {
		body: filename.clone(),
		formatted: None,
		filename: Some(filename),
		source: MediaSource::Plain(mxc.to_string().into()),
		info: Some(Box::new(FileInfo {
			mimetype: Some(mime.to_owned()),
			size: Some(ruma_from_usize(text.len())),
			..Default::default()
		})),
	};

	Ok(RoomMessageEventContent::new(MessageType::File(file)))
}
