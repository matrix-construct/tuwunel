use std::{
	collections::BTreeMap,
	fmt,
	sync::{Arc, Mutex},
};

use tracing::{
	Level,
	field::{Field, Visit},
	span::Id,
};
use tracing_core::{Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan};

pub struct CaptureManager {
	captures: Mutex<Vec<Capture>>,
}

struct Capture {
	span_id: Id,
	events: Vec<EventData>,
}

type Values = BTreeMap<&'static str, String>;

pub struct EventData {
	pub level: Level,
	pub span_name: &'static str,
	pub values: Values,
}

impl EventData {
	pub fn message(&self) -> &str {
		self.values
			.get("message")
			.map(String::as_str)
			.unwrap_or("")
	}
}

impl Default for CaptureManager {
	fn default() -> Self { Self::new() }
}

impl CaptureManager {
	#[must_use]
	pub fn new() -> Self { Self { captures: Mutex::new(Vec::new()) } }

	pub fn start_capture(&self, span_id: &Id) {
		let capture = Capture {
			span_id: span_id.clone(),
			events: Vec::new(),
		};
		self.captures.lock().unwrap().push(capture);
	}

	pub fn stop_capture(&self, span_id: &Id) -> Vec<EventData> {
		let mut vec = self.captures.lock().unwrap();
		let pos = vec
			.iter()
			.position(|capture| capture.span_id == *span_id)
			.expect("capture not found");
		let capture = vec.swap_remove(pos);
		capture.events
	}
}

pub struct CaptureLayer {
	manager: Arc<CaptureManager>,
}

impl CaptureLayer {
	pub fn new(manager: &Arc<CaptureManager>) -> Self { Self { manager: manager.clone() } }
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
		if !event
			.metadata()
			.module_path()
			.is_some_and(|module_path| module_path.starts_with(crate::info::CRATE_PREFIX))
		{
			return;
		}

		let mut vec = self.manager.captures.lock().unwrap();

		let Some(event_span) = ctx.event_span(event) else {
			return;
		};

		let Some(capture) = vec.iter_mut().find(|capture| {
			event_span
				.scope()
				.any(|span| capture.span_id == span.id())
		}) else {
			return;
		};

		let mut visitor = Visitor { values: Values::new() };
		event.record(&mut visitor);

		capture.events.push(EventData {
			level: event.metadata().level().to_owned(),
			span_name: ctx.current_span().metadata().unwrap().name(),
			values: visitor.values,
		});
	}
}

struct Visitor {
	values: Values,
}

impl Visit for Visitor {
	fn record_debug(&mut self, f: &Field, v: &dyn fmt::Debug) {
		self.values.insert(f.name(), format!("{v:?}"));
	}

	fn record_str(&mut self, f: &Field, v: &str) { self.values.insert(f.name(), v.to_owned()); }
}
