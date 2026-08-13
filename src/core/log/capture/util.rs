use std::sync::{Arc, Mutex};

use super::{
	super::{Level, fmt},
	Closure, Data,
};
use crate::Result;

/// Builds a capture callback that appends HTML log lines.
///
/// Each event locks the shared output and formats its level, current span, and
/// message. The callback owns the supplied output handle and panics if the lock
/// is poisoned or formatting fails.
pub fn fmt_html<S>(out: Arc<Mutex<S>>) -> Box<Closure>
where
	S: std::fmt::Write + Send + 'static,
{
	fmt(fmt::html, out)
}

/// Builds a capture callback that appends Markdown log lines.
///
/// Each event locks the shared output and formats its level, current span, and
/// message. The callback owns the supplied output handle and panics if the lock
/// is poisoned or formatting fails.
pub fn fmt_markdown<S>(out: Arc<Mutex<S>>) -> Box<Closure>
where
	S: std::fmt::Write + Send + 'static,
{
	fmt(fmt::markdown, out)
}

/// Builds a capture callback around a compatible formatting function.
///
/// The output is locked for each event before the formatter is called. The
/// returned callback panics if the lock is poisoned or the formatter returns an
/// error.
pub fn fmt<F, S>(fun: F, out: Arc<Mutex<S>>) -> Box<Closure>
where
	F: Fn(&mut S, &Level, &str, &str) -> Result + Send + Sync + Copy + 'static,
	S: std::fmt::Write + Send + 'static,
{
	Box::new(move |data| call(fun, &mut *out.lock().expect("locked"), &data))
}

fn call<F, S>(fun: F, out: &mut S, data: &Data<'_>)
where
	F: Fn(&mut S, &Level, &str, &str) -> Result,
	S: std::fmt::Write,
{
	fun(out, &data.level(), data.span_name(), data.message()).expect("log line appended");
}
