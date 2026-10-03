use serde::Deserialize;

use super::Event;

/// The `msgtype` of an `m.room.message` event's content.
///
/// Each type the specification defines has a variant, and any other value,
/// custom or unstable, reads as `Other`, so reading one never allocates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum MsgType {
	/// An audio message, `m.audio`.
	#[serde(rename = "m.audio")]
	Audio,

	/// An action performed by the sender, `m.emote`.
	#[serde(rename = "m.emote")]
	Emote,

	/// A file message, `m.file`.
	#[serde(rename = "m.file")]
	File,

	/// An image message, `m.image`.
	#[serde(rename = "m.image")]
	Image,

	/// An in-room key verification request, `m.key.verification.request`.
	#[serde(rename = "m.key.verification.request")]
	KeyVerificationRequest,

	/// A location message, `m.location`.
	#[serde(rename = "m.location")]
	Location,

	/// An automated message no client may answer automatically, `m.notice`.
	#[serde(rename = "m.notice")]
	Notice,

	/// A notice from the server itself, `m.server_notice`.
	#[serde(rename = "m.server_notice")]
	ServerNotice,

	/// A plain text message, `m.text`.
	#[serde(rename = "m.text")]
	Text,

	/// A video message, `m.video`.
	#[serde(rename = "m.video")]
	Video,

	/// Any type the specification does not define.
	#[serde(other)]
	Other,
}

#[derive(Deserialize)]
struct Content {
	msgtype: Option<MsgType>,
}

pub(super) fn content_msgtype<E: Event>(event: &E) -> Option<MsgType> {
	event
		.get_content()
		.ok()
		.and_then(|content: Content| content.msgtype)
}
