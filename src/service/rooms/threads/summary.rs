//! The `m.thread` summary a thread root stores in its `unsigned`.
//!
//! The functions here build, read and rewrite the bundle the threads service
//! keeps on the root's accepted row, and render a reply as its latest event.

use std::collections::BTreeMap;

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, UInt, events::AnySyncMessageLikeEvent, serde::Raw,
	uint,
};
use tuwunel_matrix::{Event, Pdu};

const BUNDLE_PATH: [&str; 3] = ["unsigned", "m.relations", "m.thread"];

/// Records `event` as the newest reply in a root's `unsigned`.
///
/// Sets the bundle's `latest_event`, counts the reply, and marks the thread as
/// participated in only when the bundle does not say either way.
pub(super) fn update_thread_bundle<E>(unsigned: &mut CanonicalJsonObject, event: &E)
where
	E: Event,
{
	update_thread_bundle_raw(unsigned, &event.to_sync_message_like_without_unsigned());
}

pub(super) fn update_thread_bundle_raw(
	unsigned: &mut CanonicalJsonObject,
	latest_event: &Raw<AnySyncMessageLikeEvent>,
) {
	add_reply(
		unsigned,
		canonical_json(latest_event).expect("thread latest event should be canonical JSON"),
	);
}

pub(super) fn thread_reply_json(pdu: &Pdu) -> Option<CanonicalJsonValue> {
	canonical_json(&pdu.to_sync_message_like_without_unsigned()).ok()
}

/// Whether a root's `m.thread` summary still counts a reply.
///
/// A summary recounted to zero lists no thread; one without a count is taken to
/// count a reply.
pub(super) fn counts_replies(root: &CanonicalJsonObject) -> bool {
	thread_bundle(root).is_some_and(|thread| bundle_count(thread) != Some(uint!(0)))
}

pub(super) fn thread_count(root: &CanonicalJsonObject) -> Option<UInt> {
	thread_bundle(root).and_then(bundle_count)
}

pub(super) fn thread_bundle(root: &CanonicalJsonObject) -> Option<&CanonicalJsonObject> {
	BUNDLE_PATH
		.into_iter()
		.try_fold(root, |object, field| object.get(field)?.as_object())
}

pub(super) fn thread_latest(root: &CanonicalJsonObject) -> Option<&str> {
	thread_bundle(root)?
		.get("latest_event")?
		.as_object()?
		.get("event_id")?
		.as_str()
}

pub(super) fn set_thread_count(
	mut root: CanonicalJsonObject,
	count: Option<UInt>,
) -> CanonicalJsonObject {
	if let Some((thread, count)) = thread_bundle_mut(&mut root).zip(count) {
		thread.insert("count".into(), CanonicalJsonValue::Integer(count.into()));
	}

	root
}

/// Borrows the stored thread bundle for a summary rewrite.
///
/// Returns `None` when any component is missing or is not an object.
pub(crate) fn thread_bundle_mut(
	root: &mut CanonicalJsonObject,
) -> Option<&mut CanonicalJsonObject> {
	BUNDLE_PATH
		.into_iter()
		.try_fold(root, |object, field| object.get_mut(field)?.as_object_mut())
}

pub(super) fn set_thread_latest(
	mut root: CanonicalJsonObject,
	latest: Option<CanonicalJsonValue>,
) -> CanonicalJsonObject {
	let Some(latest) = latest else {
		return remove_thread_bundle(root);
	};

	if let Some(thread) = thread_bundle_mut(&mut root) {
		thread.insert("latest_event".into(), latest);
	}

	root
}

pub(super) fn omit_thread_latest(mut root: CanonicalJsonObject) -> CanonicalJsonObject {
	if thread_count(&root).is_none_or(|count| count == uint!(0)) {
		return remove_thread_bundle(root);
	}

	if let Some(thread) = thread_bundle_mut(&mut root) {
		thread.remove("latest_event");
	}

	root
}

fn canonical_json(
	event: &Raw<AnySyncMessageLikeEvent>,
) -> serde_json::Result<CanonicalJsonValue> {
	serde_json::from_str(event.json().get())
}

fn add_reply(unsigned: &mut CanonicalJsonObject, latest_event: CanonicalJsonValue) {
	let thread = BUNDLE_PATH
		.into_iter()
		.skip(1)
		.fold(unsigned, canonical_object_field);

	let count = bundle_count(thread)
		.unwrap_or_default()
		.saturating_add(uint!(1));

	thread.insert("latest_event".into(), latest_event);
	thread.insert("count".into(), CanonicalJsonValue::Integer(count.into()));

	if !matches!(thread.get("current_user_participated"), Some(CanonicalJsonValue::Bool(_))) {
		thread.insert("current_user_participated".into(), CanonicalJsonValue::Bool(true));
	}
}

fn canonical_object_field<'a>(
	object: &'a mut CanonicalJsonObject,
	field: &str,
) -> &'a mut CanonicalJsonObject {
	if !matches!(object.get(field), Some(CanonicalJsonValue::Object(_))) {
		object.insert(field.into(), CanonicalJsonValue::Object(BTreeMap::new()));
	}

	object
		.get_mut(field)
		.and_then(CanonicalJsonValue::as_object_mut)
		.expect("canonical object field was initialized as an object")
}

fn bundle_count(thread: &CanonicalJsonObject) -> Option<UInt> {
	thread
		.get("count")?
		.as_integer()
		.map(i64::from)
		.and_then(|count| count.try_into().ok())
}

fn remove_thread_bundle(mut root: CanonicalJsonObject) -> CanonicalJsonObject {
	let Some(unsigned) = root
		.get_mut("unsigned")
		.and_then(CanonicalJsonValue::as_object_mut)
	else {
		return root;
	};

	let Some(relations) = unsigned
		.get_mut("m.relations")
		.and_then(CanonicalJsonValue::as_object_mut)
	else {
		return root;
	};

	relations.remove("m.thread");
	if relations.is_empty() {
		unsigned.remove("m.relations");
	}

	root
}
