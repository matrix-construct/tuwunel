use std::fmt::Write;

use super::{Level, color};
use crate::Result;

/// Appends one captured log event as an HTML line.
///
/// The level receives its level-specific Matrix color and each component is
/// placed in a code element. A line break terminates the rendered entry.
pub fn html<S>(out: &mut S, level: &Level, span: &str, msg: &str) -> Result
where
	S: Write + ?Sized,
{
	let color = color::code_tag(level);
	let level = level.as_str().to_uppercase();
	write!(
		out,
		"<font data-mx-color=\"{color}\"><code>{level:>5}</code></font> <code>{span:^12}</code> \
		 <code>{msg}</code><br>"
	)?;

	Ok(())
}

/// Appends one captured log event as a Markdown line.
///
/// Level, span, and message are rendered as inline-code columns separated by
/// spaces. A newline terminates the entry.
pub fn markdown<S>(out: &mut S, level: &Level, span: &str, msg: &str) -> Result
where
	S: Write + ?Sized,
{
	let level = level.as_str().to_uppercase();
	writeln!(out, "`{level:>5}` `{span:^12}` `{msg}`")?;

	Ok(())
}

/// Appends one captured log event as a Markdown table row.
///
/// The level and span receive fixed-width alignment while the message remains
/// left aligned. A newline terminates the row.
pub fn markdown_table<S>(out: &mut S, level: &Level, span: &str, msg: &str) -> Result
where
	S: Write + ?Sized,
{
	let level = level.as_str().to_uppercase();
	writeln!(out, "| {level:>5} | {span:^12} | {msg} |")?;

	Ok(())
}

/// Appends the header for a captured-log Markdown table.
///
/// The header defines level, span, and message columns together with their
/// alignment row. Callers can append rows with `markdown_table`.
pub fn markdown_table_head<S>(out: &mut S) -> Result
where
	S: Write + ?Sized,
{
	write!(out, "| level | span | message |\n| ------: | :-----: | :------- |\n")?;

	Ok(())
}
