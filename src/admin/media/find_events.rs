use std::collections::HashSet;

use futures::{Stream, TryStreamExt, pin_mut};
use serde_json::Value;
use tuwunel_core::{Err, Result};

use crate::admin_command;

struct FoundEvent {
	event_id: String,
	room_id: Option<String>,
	sender: Option<String>,
	kind: Option<String>,
	source: &'static str,
}

#[admin_command]
pub(super) async fn find_events(&self, file: String) -> Result {
	let mxcs = self
		.services
		.media
		.mxcs_from_file_name(&file)
		.await?;

	if mxcs.is_empty() {
		return Err!("No media found for filename {file:?}");
	}

	let targets: HashSet<String> = mxcs.iter().map(ToString::to_string).collect();

	let mut seen = HashSet::new();
	let mut found = Vec::new();

	scan(self.services.timeline.pdus_raw(), "timeline", &targets, &mut seen, &mut found).await?;

	scan(
		self.services.timeline.outlier_pdus_raw(),
		"outlier",
		&targets,
		&mut seen,
		&mut found,
	)
	.await?;

	scan(
		self.services.retention.retained_pdus_raw(),
		"retained",
		&targets,
		&mut seen,
		&mut found,
	)
	.await?;

	let mxcs = mxcs
		.iter()
		.map(ToString::to_string)
		.collect::<Vec<_>>()
		.join(", ");

	write!(self, "Found {} event(s) referencing {mxcs}:\n\n```\n", found.len()).await?;

	for event in found {
		writeln!(
			self,
			"{}  {}  {}  {}  [{}]",
			event.event_id,
			event.room_id.as_deref().unwrap_or("-"),
			event.sender.as_deref().unwrap_or("-"),
			event.kind.as_deref().unwrap_or("-"),
			event.source,
		)
		.await?;
	}

	write!(self, "```").await
}

async fn scan<'a, S>(
	stream: S,
	source: &'static str,
	targets: &HashSet<String>,
	seen: &mut HashSet<String>,
	found: &mut Vec<FoundEvent>,
) -> Result
where
	S: Stream<Item = Result<&'a [u8]>> + Send,
{
	pin_mut!(stream);

	while let Some(raw) = stream.try_next().await? {
		let Ok(text) = std::str::from_utf8(raw) else {
			continue;
		};

		if !targets.iter().any(|target| text.contains(target)) {
			continue;
		}

		let Ok(value) = serde_json::from_str::<Value>(text) else {
			continue;
		};

		let Some(content) = value.get("content") else {
			continue;
		};

		if !contains_target(content, targets) {
			continue;
		}

		let Some(event) = found_event(&value, source) else {
			continue;
		};

		if seen.insert(event.event_id.clone()) {
			found.push(event);
		}
	}

	Ok(())
}

fn contains_target(value: &Value, targets: &HashSet<String>) -> bool {
	match value {
		| Value::String(string) => targets.contains(string),
		| Value::Array(items) => items
			.iter()
			.any(|item| contains_target(item, targets)),
		| Value::Object(map) => map
			.values()
			.any(|value| contains_target(value, targets)),
		| _ => false,
	}
}

fn found_event(value: &Value, source: &'static str) -> Option<FoundEvent> {
	let string = |key: &str| {
		value
			.get(key)
			.and_then(Value::as_str)
			.map(ToOwned::to_owned)
	};

	Some(FoundEvent {
		event_id: string("event_id")?,
		room_id: string("room_id"),
		sender: string("sender"),
		kind: string("type"),
		source,
	})
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use serde_json::json;

	use super::{contains_target, found_event};

	fn targets() -> HashSet<String> {
		["mxc://example.org/media".to_owned()]
			.into_iter()
			.collect()
	}

	#[test]
	fn nested_content_is_searched() {
		let content = json!({
			"body": "caption",
			"info": { "thumbnail_url": "mxc://example.org/media" },
			"file": { "url": "mxc://example.org/media" },
		});

		assert!(contains_target(&content, &targets()));
	}

	#[test]
	fn unrelated_content_is_rejected() {
		let content = json!({
			"body": "mxc://example.org/other",
			"info": { "thumbnail_url": "mxc://example.org/other" },
		});

		assert!(!contains_target(&content, &targets()));
	}

	#[test]
	fn event_fields_are_extracted() {
		let value = json!({
			"event_id": "$event:example.org",
			"room_id": "!room:example.org",
			"sender": "@alice:example.org",
			"type": "m.room.message",
		});

		let event = found_event(&value, "timeline").expect("event fields");

		assert_eq!(event.event_id, "$event:example.org");
		assert_eq!(event.room_id.as_deref(), Some("!room:example.org"));
		assert_eq!(event.sender.as_deref(), Some("@alice:example.org"));
		assert_eq!(event.kind.as_deref(), Some("m.room.message"));
	}

	#[test]
	fn event_id_is_required() {
		let value = json!({ "room_id": "!room:example.org" });

		assert!(found_event(&value, "timeline").is_none());
	}
}
