use std::{env, io, sync::LazyLock};

use tracing::{
	Event, Level, Subscriber,
	field::{Field, Visit},
};
use tracing_subscriber::{
	field::RecordFields,
	fmt,
	fmt::{
		FmtContext, FormatEvent, FormatFields, MakeWriter,
		format::{Compact, DefaultVisitor, Format, Full, Pretty, Writer},
	},
	registry::LookupSpan,
};

use crate::{Config, Result, apply};

static SYSTEMD_MODE: LazyLock<bool> =
	LazyLock::new(|| env::var("SYSTEMD_EXEC_PID").is_ok() && env::var("JOURNAL_STREAM").is_ok());

pub struct ConsoleWriter {
	stdout: io::Stdout,
	stderr: io::Stderr,
	_journal_stream: [u64; 2],
	use_stderr: bool,
}

impl ConsoleWriter {
	#[must_use]
	pub fn new(_config: &Config) -> Self {
		let journal_stream = get_journal_stream();
		Self {
			stdout: io::stdout(),
			stderr: io::stderr(),
			_journal_stream: journal_stream.into(),
			use_stderr: journal_stream.0 != 0,
		}
	}
}

impl<'a> MakeWriter<'a> for ConsoleWriter {
	type Writer = &'a Self;

	fn make_writer(&'a self) -> Self::Writer {
		self
	}
}

impl io::Write for &'_ ConsoleWriter {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		if self.use_stderr {
			self.stderr.lock().write(buf)
		} else {
			self.stdout.lock().write(buf)
		}
	}

	fn flush(&mut self) -> io::Result<()> {
		if self.use_stderr {
			self.stderr.lock().flush()
		} else {
			self.stdout.lock().flush()
		}
	}
}

pub struct ConsoleFormat {
	_compact: Format<Compact>,
	full: Format<Full>,
	pretty: Format<Pretty>,
}

impl ConsoleFormat {
	#[must_use]
	pub fn new(config: &Config) -> Self {
		Self {
			_compact: fmt::format().compact(),

			full: Format::<Full>::default()
				.with_thread_ids(config.log_thread_ids)
				.with_ansi(config.log_colors),

			pretty: fmt::format()
				.pretty()
				.with_ansi(config.log_colors)
				.with_thread_names(true)
				.with_thread_ids(true)
				.with_target(true)
				.with_file(true)
				.with_line_number(true)
				.with_source_location(true),
		}
	}
}

impl<S, N> FormatEvent<S, N> for ConsoleFormat
where
	S: Subscriber + for<'a> LookupSpan<'a>,
	N: for<'a> FormatFields<'a> + 'static,
{
	fn format_event(
		&self,
		ctx: &FmtContext<'_, S, N>,
		writer: Writer<'_>,
		event: &Event<'_>,
	) -> Result<(), std::fmt::Error> {
		let is_debug = cfg!(debug_assertions)
			&& event
				.fields()
				.any(|field| field.name() == "_debug");

		match *event.metadata().level() {
			| Level::ERROR if !is_debug => self.pretty.format_event(ctx, writer, event),
			| _ => self.full.format_event(ctx, writer, event),
		}
	}
}

struct ConsoleVisitor<'a> {
	visitor: DefaultVisitor<'a>,
}

impl<'writer> FormatFields<'writer> for ConsoleFormat {
	fn format_fields<R>(&self, writer: Writer<'writer>, fields: R) -> Result<(), std::fmt::Error>
	where
		R: RecordFields,
	{
		let mut visitor = ConsoleVisitor {
			visitor: DefaultVisitor::<'_>::new(writer, true),
		};

		fields.record(&mut visitor);

		Ok(())
	}
}

impl Visit for ConsoleVisitor<'_> {
	fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
		if field.name().starts_with('_') {
			return;
		}

		self.visitor.record_debug(field, value);
	}
}

#[must_use]
fn get_journal_stream() -> (u64, u64) {
	is_systemd_mode()
		.then(|| env::var("JOURNAL_STREAM").ok())
		.flatten()
		.as_deref()
		.and_then(|s| s.split_once(':'))
		.map(apply!(2, str::parse))
		.map(apply!(2, Result::unwrap_or_default))
		.unwrap_or((0, 0))
}

#[inline]
#[must_use]
pub fn is_systemd_mode() -> bool {
	*SYSTEMD_MODE
}
