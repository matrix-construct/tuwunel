use tracing::Level;
use tracing_core::{Event, span::Current};

use super::{Layer, layer::Value};
use crate::{info, utils::string::EMPTY};

pub struct Data<'a> {
	pub layer: &'a Layer,
	pub event: &'a Event<'a>,
	pub current: &'a Current,
	pub values: &'a [Value],
	pub scope: &'a [&'static str],
}

impl Data<'_> {
	#[must_use]
	pub fn our_modules(&self) -> bool {
		self.mod_name().starts_with(info::CRATE_PREFIX)
	}

	#[must_use]
	pub fn level(&self) -> Level {
		*self.event.metadata().level()
	}

	#[must_use]
	pub fn mod_name(&self) -> &str {
		self.event
			.metadata()
			.module_path()
			.unwrap_or_default()
	}

	#[must_use]
	pub fn span_name(&self) -> &str {
		self.current
			.metadata()
			.map_or(EMPTY, |s| s.name())
	}

	#[must_use]
	pub fn message(&self) -> &str {
		self.values
			.iter()
			.find(|(k, _)| *k == "message")
			.map_or(EMPTY, |(_, v)| v.as_str())
	}
}
