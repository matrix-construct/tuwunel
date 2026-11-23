//! HTML text escaping utilities.
//!
//! The helpers replace markup-sensitive ASCII characters with entity
//! references. An existing `&amp;` becomes `&amp;amp;`.

use std::borrow::Cow;

/// Escapes a value bound for a page template filled by placeholder
/// replacement.
///
/// Braces become entity references as well as the characters `escape`
/// replaces, so the value cannot carry a `{placeholder}` that a later
/// replacement would fill. Text needing no escape is borrowed.
#[must_use]
pub fn escape_template_value(s: &str) -> Cow<'_, str> {
	escape_with(s, |c| match c {
		| '{' => Some("&#123;"),
		| '}' => Some("&#125;"),
		| c => markup_entity(c),
	})
}

/// Escapes HTML-sensitive ASCII characters in a string.
///
/// Ampersands, angle brackets, both quote characters, and backticks are
/// replaced with entity references. Existing entity references are escaped
/// again.
#[must_use]
pub fn escape(s: &str) -> String { escape_with(s, markup_entity).into_owned() }

fn escape_with<Entity>(s: &str, entity: Entity) -> Cow<'_, str>
where
	Entity: Fn(char) -> Option<&'static str>,
{
	let Some(start) = s.find(|c: char| entity(c).is_some()) else {
		return s.into();
	};

	let (clean, rest) = s.split_at(start);

	rest.chars()
		.fold(buffer(clean, s.len()), |mut out, c| {
			match entity(c) {
				| Some(entity) => out.push_str(entity),
				| None => out.push(c),
			}

			out
		})
		.into()
}

fn buffer(prefix: &str, capacity: usize) -> String {
	let mut out = String::with_capacity(capacity);
	out.push_str(prefix);
	out
}

fn markup_entity(c: char) -> Option<&'static str> {
	match c {
		| '&' => Some("&amp;"),
		| '<' => Some("&lt;"),
		| '>' => Some("&gt;"),
		| '"' => Some("&quot;"),
		| '\'' => Some("&#x27;"),
		| '`' => Some("&#x60;"),
		| _ => None,
	}
}
