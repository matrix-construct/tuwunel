use std::{fmt, sync::Arc};

use arrayvec::ArrayVec;
use tracing::field::{Field, Visit};
use tracing_core::{Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan};

use super::{Capture, Data, State};

pub struct Layer {
	state: Arc<State>,
}

struct Visitor {
	values: Values,
}

type Values = ArrayVec<Value, 32>;
pub type Value = (&'static str, String);

type ScopeNames = ArrayVec<&'static str, 32>;

impl Layer {
	#[inline]
	pub fn new(state: &Arc<State>) -> Self {
		Self { state: state.clone() }
	}
}

impl fmt::Debug for Layer {
	#[inline]
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.debug_struct("capture::Layer").finish()
	}
}

impl<S> tracing_subscriber::Layer<S> for Layer
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
		self.state
			.active
			.read()
			.expect("shared lock")
			.iter()
			.filter(|capture| filter(self, capture, event, &ctx))
			.for_each(|capture| handle(self, capture, event, &ctx));
	}
}

fn handle<S>(layer: &Layer, capture: &Capture, event: &Event<'_>, ctx: &Context<'_, S>)
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	let names = ScopeNames::new();
	let mut visitor = Visitor { values: Values::new() };
	event.record(&mut visitor);

	let mut closure = capture.closure.lock().expect("exclusive lock");
	closure(Data {
		layer,
		event,
		current: &ctx.current_span(),
		values: &visitor.values,
		scope: &names,
	});
}

fn filter<S>(layer: &Layer, capture: &Capture, event: &Event<'_>, ctx: &Context<'_, S>) -> bool
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	let values = Values::new();
	let mut names = ScopeNames::new();
	if let Some(scope) = ctx.event_scope(event) {
		for span in scope {
			names.push(span.name());
		}
	}

	capture.filter.as_ref().is_none_or(|filter| {
		filter(Data {
			layer,
			event,
			current: &ctx.current_span(),
			values: &values,
			scope: &names,
		})
	})
}

impl Visit for Visitor {
	fn record_debug(&mut self, f: &Field, v: &dyn fmt::Debug) {
		self.values.push((f.name(), format!("{v:?}")));
	}

	fn record_str(&mut self, f: &Field, v: &str) {
		self.values.push((f.name(), v.to_owned()));
	}
}
