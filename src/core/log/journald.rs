use std::{
	cell::RefCell,
	env::args_os,
	ffi::OsStr,
	fmt::Debug,
	io::{self, Write, stderr},
	os::unix::net::UnixDatagram,
	path::Path,
};

use tracing::{
	Event, Level, Metadata, Subscriber,
	field::{Field, Visit},
	level_filters::LevelFilter,
	span::{Attributes, Id, Record},
	subscriber::Interest,
};
use tracing_subscriber::{
	layer::{Context, Layer},
	registry::LookupSpan,
};

use super::is_systemd_mode;
use crate::{
	Config, Result,
	arrayvec::ArrayString,
	err, format_small_string, implement,
	smallstr::SmallString,
	smallvec::SmallVec,
	utils::{
		math::{ExpectInto, Expected},
		string::{Unquote, to_array_string},
	},
};

#[cfg(test)]
mod tests;

/// Encoded journal fields; a longer run spills to the heap.
type Buffer = SmallVec<[u8; 128]>;

/// Syslog identifier tagging every entry.
type Identifier = SmallString<[u8; 16]>;

/// Formatted source line number.
type CodeLine = ArrayString<10>;

/// Journal name of a user field, including the prefix.
type FieldName = SmallString<[u8; 32]>;

/// Value of a user field recorded through its `Debug` implementation.
type FieldValue = SmallString<[u8; 64]>;

const SOCKET: &str = "/run/systemd/journal/socket";

const IDENTIFIER: &str = "tuwunel";

/// Keeps a user field from colliding with one of the journal's own.
const PREFIX: &str = "F_";

/// Journald rejects a longer field name.
const NAME_MAX: usize = 64;

/// Bounds the datagram well under the default socket send buffer.
const PAYLOAD_MAX: usize = 128 * 1024;

const LEN_PREFIX: usize = size_of::<u64>();

/// Datagram connection to the journald submission socket.
pub struct Journal {
	socket: UnixDatagram,
	identifier: Identifier,
}

/// One entry, submitted when the writer is dropped. Its payload accumulates
/// in the thread's buffer, which the fields layer opened for this event.
pub struct Entry<'a> {
	journal: &'a Journal,
	message: usize,
}

/// Records the fields of each event and span for the entry the wrapped
/// console layer is about to write, leaving them queryable with
/// `journalctl F_NAME=value`.
pub struct Fields<L> {
	inner: L,
	submit: bool,
}

/// Encoded fields of one span, held in its registry extensions.
struct SpanFields(Buffer);

struct Visitor<'a> {
	fields: &'a mut Buffer,
}

thread_local! {
	/// Entry under construction on this thread, reused across events.
	static PAYLOAD: RefCell<Buffer> = const { RefCell::new(Buffer::new_const()) };
}

impl Journal {
	/// Opens the socket when journald is configured, keeping the console when
	/// nothing is listening on it.
	#[must_use]
	pub fn open(config: &Config) -> Option<Self> {
		enabled(config)
			.then(Self::new)?
			.inspect_err(|e| {
				writeln!(stderr(), "{e}").ok();
			})
			.ok()
	}

	fn new() -> Result<Self> {
		let journal = Self {
			socket: UnixDatagram::unbound()?,
			identifier: identifier(),
		};

		journal
			.send(&[])
			.map_err(|e| err!(Config("log_journald", "{SOCKET}: {e}.")))?;

		Ok(journal)
	}
}

/// Tags entries with the running executable's name.
fn identifier() -> Identifier {
	args_os()
		.next()
		.as_deref()
		.map(Path::new)
		.and_then(Path::file_name)
		.and_then(OsStr::to_str)
		.map_or_else(|| IDENTIFIER.into(), Into::into)
}

/// Opens an entry at the event's severity, carrying its source metadata after
/// the fields recorded for it, and leaves the message field open.
#[implement(Journal)]
#[must_use]
pub fn entry(&self, meta: &Metadata<'_>) -> Entry<'_> {
	let message = PAYLOAD.with_borrow_mut(|payload| {
		put(payload, "PRIORITY", &[priority(*meta.level())]);
		put(payload, "SYSLOG_IDENTIFIER", self.identifier.as_bytes());
		put(payload, "TARGET", meta.target().as_bytes());

		if let Some(file) = meta.file() {
			put(payload, "CODE_FILE", file.as_bytes());
		}

		if let Some(line) = meta.line() {
			let line: CodeLine = to_array_string(line);

			put(payload, "CODE_LINE", line.as_bytes());
		}

		payload.extend_from_slice(b"MESSAGE\n");
		payload.extend_from_slice(&[0; LEN_PREFIX]);

		payload.len()
	});

	Entry { journal: self, message }
}

#[implement(Journal)]
fn send(&self, payload: &[u8]) -> io::Result<usize> { self.socket.send_to(payload, SOCKET) }

impl Write for Entry<'_> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		PAYLOAD.with_borrow_mut(|payload| payload.extend_from_slice(buf));

		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl Drop for Entry<'_> {
	fn drop(&mut self) {
		PAYLOAD.with_borrow_mut(|payload| {
			close(payload, self.message);

			if self.journal.send(payload).is_err() {
				fallback(&payload[self.message..]);
			}

			// A span event reaches the writer without passing the fields layer,
			// so the buffer is released here rather than after recording.
			payload.clear();
		});
	}
}

/// Writes a message the socket would not take to the console, which journald
/// captures in turn under systemd.
fn fallback(message: &[u8]) { stderr().write_all(message).ok(); }

/// Seals the message field, trimming the formatter's trailing whitespace and
/// truncating on a character boundary what the datagram cannot carry.
#[expect(
	clippy::little_endian_bytes,
	reason = "the journal protocol specifies little-endian field lengths"
)]
fn close(payload: &mut Buffer, message: usize) {
	let budget = PAYLOAD_MAX.saturating_sub(message);
	let len = payload[message..].trim_ascii_end().len();
	let len = (len > budget)
		.then(|| boundary(&payload[message..], budget))
		.unwrap_or(len);

	let size: u64 = len.expect_into();

	payload.truncate(message.expected_add(len));
	payload[message.expected_sub(LEN_PREFIX)..message].copy_from_slice(&size.to_le_bytes());
	payload.push(b'\n');
}

/// Steps back to the nearest character boundary at or below `len`; a
/// continuation byte never begins a character.
fn boundary(message: &[u8], len: usize) -> usize {
	(0..=len)
		.rev()
		.find(|&i| {
			message
				.get(i)
				.is_none_or(|byte| byte & 0b1100_0000 != 0b1000_0000)
		})
		.unwrap_or_default()
}

impl<L> Fields<L> {
	/// Wraps a subscriber layer with optional native journal field recording.
	///
	/// Events always continue through the inner layer. Structured field
	/// submission is enabled only when the supplied configuration selects the
	/// journal.
	pub fn new(inner: L, config: &Config) -> Self { Self { inner, submit: enabled(config) } }
}

/// Whether events are submitted to journald rather than written to the
/// console, which journald otherwise captures at a single fixed priority.
pub(super) fn enabled(config: &Config) -> bool { config.log_journald && is_systemd_mode() }

impl<S, L> Layer<S> for Fields<L>
where
	S: Subscriber + for<'a> LookupSpan<'a>,
	L: Layer<S>,
{
	#[inline]
	fn on_layer(&mut self, subscriber: &mut S) { self.inner.on_layer(subscriber); }

	#[inline]
	fn register_callsite(&self, meta: &'static Metadata<'static>) -> Interest {
		self.inner.register_callsite(meta)
	}

	#[inline]
	fn enabled(&self, meta: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
		self.inner.enabled(meta, ctx)
	}

	#[inline]
	fn max_level_hint(&self) -> Option<LevelFilter> { self.inner.max_level_hint() }

	fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
		if self.submit {
			record_span(attrs, id, &ctx);
		}

		self.inner.on_new_span(attrs, id, ctx);
	}

	fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
		if self.submit {
			record_values(id, values, &ctx);
		}

		self.inner.on_record(id, values, ctx);
	}

	#[inline]
	fn on_follows_from(&self, id: &Id, follows: &Id, ctx: Context<'_, S>) {
		self.inner.on_follows_from(id, follows, ctx);
	}

	#[inline]
	fn event_enabled(&self, event: &Event<'_>, ctx: Context<'_, S>) -> bool {
		self.inner.event_enabled(event, ctx)
	}

	fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
		if self.submit {
			record_event(event, &ctx);
		}

		self.inner.on_event(event, ctx);
	}

	#[inline]
	fn on_enter(&self, id: &Id, ctx: Context<'_, S>) { self.inner.on_enter(id, ctx); }

	#[inline]
	fn on_exit(&self, id: &Id, ctx: Context<'_, S>) { self.inner.on_exit(id, ctx); }

	#[inline]
	fn on_close(&self, id: Id, ctx: Context<'_, S>) { self.inner.on_close(id, ctx); }

	#[inline]
	fn on_id_change(&self, old: &Id, new: &Id, ctx: Context<'_, S>) {
		self.inner.on_id_change(old, new, ctx);
	}
}

/// Encodes a span's fields once, for every event it later encloses.
fn record_span<S>(attrs: &Attributes<'_>, id: &Id, ctx: &Context<'_, S>)
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	let Some(span) = ctx.span(id) else {
		return;
	};

	let mut fields = Buffer::new();

	put(&mut fields, "SPAN_NAME", span.name().as_bytes());
	attrs.record(&mut Visitor { fields: &mut fields });

	span.extensions_mut().insert(SpanFields(fields));
}

/// Appends the fields of a span recorded after its creation.
fn record_values<S>(id: &Id, values: &Record<'_>, ctx: &Context<'_, S>)
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	let Some(span) = ctx.span(id) else {
		return;
	};

	let mut extensions = span.extensions_mut();
	let Some(SpanFields(fields)) = extensions.get_mut::<SpanFields>() else {
		return;
	};

	values.record(&mut Visitor { fields });
}

/// Opens this thread's buffer with the event's fields and those of the spans
/// enclosing it, for the entry the console writer appends to next.
fn record_event<S>(event: &Event<'_>, ctx: &Context<'_, S>)
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	PAYLOAD.with_borrow_mut(|payload| {
		payload.clear();

		extend_scope(payload, event, ctx);
		event.record(&mut Visitor { fields: payload });
	});
}

/// Appends the fields of every span enclosing the event, innermost first.
fn extend_scope<S>(payload: &mut Buffer, event: &Event<'_>, ctx: &Context<'_, S>)
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	ctx.event_scope(event)
		.into_iter()
		.flatten()
		.for_each(|span| {
			let extensions = span.extensions();

			if let Some(SpanFields(fields)) = extensions.get::<SpanFields>() {
				payload.extend_from_slice(fields);
			}
		});
}

impl Visit for Visitor<'_> {
	fn record_str(&mut self, field: &Field, value: &str) { self.record(field, value.as_bytes()); }

	fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
		let value: FieldValue = format_small_string!("{value:?}");

		self.record(field, value.as_str().unquote_infallible().as_bytes());
	}
}

/// The message is the entry's own `MESSAGE`, and an underscored name marks a
/// field the console formatter also hides.
#[implement(Visitor, params = "<'_>")]
fn record(&mut self, field: &Field, value: &[u8]) {
	let name = field.name();
	let skip = name == "message" || name.starts_with('_') || self.fields.len() >= PAYLOAD_MAX;

	if skip {
		return;
	}

	put(self.fields, &sanitize(name), value);
}

/// Journald accepts only uppercase alphanumerics and underscores in a field
/// name, and drops a field whose name it rejects.
fn sanitize(name: &str) -> FieldName {
	PREFIX
		.chars()
		.chain(
			name.chars()
				.map(|c| if matches!(c, '.' | '-') { '_' } else { c })
				.filter(|c| *c == '_' || c.is_ascii_alphanumeric())
				.map(|c| c.to_ascii_uppercase()),
		)
		.take(NAME_MAX)
		.collect()
}

/// Appends a length-encoded field, which may carry any byte sequence.
#[expect(
	clippy::little_endian_bytes,
	reason = "the journal protocol specifies little-endian field lengths"
)]
fn put(payload: &mut Buffer, name: &str, value: &[u8]) {
	let len: u64 = value.len().expect_into();

	payload.extend_from_slice(name.as_bytes());
	payload.push(b'\n');
	payload.extend_from_slice(&len.to_le_bytes());
	payload.extend_from_slice(value);
	payload.push(b'\n');
}

/// Maps a level onto the syslog severity code journald expects.
const fn priority(level: Level) -> u8 {
	match level {
		| Level::ERROR => b'3',
		| Level::WARN => b'4',
		| Level::INFO => b'5',
		| Level::DEBUG => b'6',
		| Level::TRACE => b'7',
	}
}
