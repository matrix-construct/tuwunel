use tracing::Level;
use tracing_core::{Event, span::Current};

use super::{Layer, layer::Value};
use crate::{info, utils::string::EMPTY};

/// Presents one tracing event to a capture filter or callback.
///
/// The value borrows tracing metadata and any fields recorded for the current
/// capture phase. Accessor methods provide common event attributes with empty
/// fallbacks.
pub struct Data<'a> {
	/// Capture layer that observed the event.
	///
	/// The reference identifies the subscriber layer that dispatched the event.
	pub layer: &'a Layer,

	/// Tracing event being filtered or delivered.
	///
	/// Its metadata supplies the level, target, and module path.
	pub event: &'a Event<'a>,

	/// Subscriber's current span at the time of the event.
	///
	/// Metadata is absent when no span is entered or the subscriber does not
	/// track the current span.
	pub current: &'a Current,

	/// Field names and formatted values recorded from the event.
	///
	/// Values are populated for callback delivery and can be empty during
	/// filtering.
	pub values: &'a [Value],

	/// Span names in the event's subscriber scope.
	///
	/// Scope names are populated while filtering and can be empty during
	/// callback delivery.
	pub scope: &'a [&'static str],
}

impl Data<'_> {
	/// Reports whether the event originated in a Tuwunel crate.
	///
	/// The check compares the event module path with the shared crate prefix.
	/// An event without module metadata does not match.
	#[must_use]
	pub fn our_modules(&self) -> bool { self.mod_name().starts_with(info::CRATE_PREFIX) }

	/// Returns the event's tracing level.
	///
	/// The level is copied from static tracing metadata and does not depend on
	/// the active subscriber filter.
	#[must_use]
	pub fn level(&self) -> Level { *self.event.metadata().level() }

	/// Returns the event's Rust module path.
	///
	/// Events without module metadata produce an empty string. The returned
	/// value borrows static tracing metadata.
	#[must_use]
	pub fn mod_name(&self) -> &str {
		self.event
			.metadata()
			.module_path()
			.unwrap_or_default()
	}

	/// Returns the current span's name.
	///
	/// An empty string is returned when the subscriber has no current span or
	/// no metadata for it.
	#[must_use]
	pub fn span_name(&self) -> &str {
		self.current
			.metadata()
			.map_or(EMPTY, |s| s.name())
	}

	/// Returns the event's recorded message field.
	///
	/// The first field named `message` is selected. Events without that field
	/// produce an empty string.
	#[must_use]
	pub fn message(&self) -> &str {
		self.values
			.iter()
			.find(|(k, _)| *k == "message")
			.map_or(EMPTY, |(_, v)| v.as_str())
	}
}
