//! The `m.thread` summary a thread root stores in its `unsigned`.
//!
//! These build, read and rewrite the bundle the threads service keeps on the
//! root's accepted row, and render a reply as its latest event.

use std::collections::BTreeMap;

use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, UInt, events::AnySyncMessageLikeEvent, serde::Raw,
	uint,
};
use serde_json::json;
use tuwunel_matrix::{Event, Pdu};

fn canonical_object_field<'a>(
	object: &'a mut CanonicalJsonObject,
	field: &str,
) -> &'a mut CanonicalJsonObject {
	if !matches!(object.get(field), Some(CanonicalJsonValue::Object(_))) {
		object.insert(field.into(), CanonicalJsonValue::Object(BTreeMap::new()));
	}

	let Some(CanonicalJsonValue::Object(value)) = object.get_mut(field) else {
		unreachable!("canonical object field was initialized as an object");
	};

	value
}

/// Persist a latest event whose embedded sender and event ID come from the
/// same validated event used to index the thread activity.
pub(super) fn update_thread_bundle<E>(unsigned: &mut CanonicalJsonObject, event: &E)
where
	E: Event,
{
	let latest_event = event.to_sync_message_like_without_unsigned();
	update_thread_bundle_raw(unsigned, &latest_event);
}

pub(super) fn update_thread_bundle_raw(
	unsigned: &mut CanonicalJsonObject,
	latest_event: &Raw<AnySyncMessageLikeEvent>,
) {
	let relations = canonical_object_field(unsigned, "m.relations");
	let thread = canonical_object_field(relations, "m.thread");
	let count = thread
		.get("count")
		.cloned()
		.and_then(|count| serde_json::from_value::<UInt>(count.into()).ok());

	let count = count.map_or_else(|| uint!(1), |count| count.saturating_add(uint!(1)));
	let latest_event = serde_json::from_str(latest_event.json().get())
		.expect("thread latest event should be canonical JSON");

	thread.insert("latest_event".into(), latest_event);
	thread.insert(
		"count".into(),
		json!(count)
			.try_into()
			.expect("thread count is canonical JSON"),
	);

	if !matches!(thread.get("current_user_participated"), Some(CanonicalJsonValue::Bool(_))) {
		thread.insert("current_user_participated".into(), CanonicalJsonValue::Bool(true));
	}
}

/// Whether a root's `m.thread` summary still counts a reply.
///
/// A summary recounted to zero lists no thread; one without a count is taken to
/// count a reply.
pub(super) fn counts_replies(root: &CanonicalJsonObject) -> bool {
	thread_bundle(root).is_some() && thread_count(root) != Some(uint!(0))
}

pub(super) fn thread_count(root: &CanonicalJsonObject) -> Option<UInt> {
	UInt::try_from(i64::from(thread_bundle(root)?.get("count")?.as_integer()?)).ok()
}

pub(super) fn thread_bundle(root: &CanonicalJsonObject) -> Option<&CanonicalJsonObject> {
	["unsigned", "m.relations", "m.thread"]
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
/// Missing or non-object components leave the root unchanged.
pub(crate) fn thread_bundle_mut(
	root: &mut CanonicalJsonObject,
) -> Option<&mut CanonicalJsonObject> {
	["unsigned", "m.relations", "m.thread"]
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

pub(super) fn thread_reply_json(pdu: &Pdu) -> Option<CanonicalJsonValue> {
	serde_json::from_str(
		pdu.to_sync_message_like_without_unsigned()
			.json()
			.get(),
	)
	.ok()
}
