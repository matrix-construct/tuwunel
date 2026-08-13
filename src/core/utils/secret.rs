#[cfg(test)]
mod tests;

use std::{fs::read_to_string, path::Path};

use crate::{error, smallstr::SmallString};

/// Owned string used for secret configuration values.
///
/// The type provides 64 bytes of inline capacity and spills to the heap when
/// needed. It does not redact formatting or erase its contents on drop.
pub type Secret = SmallString<[u8; 64]>;

/// Whether a secret is configured at all.
///
/// Answered without opening the file, so an unauthenticated caller cannot drive
/// filesystem work. A configured file which turns out to be unreadable or blank
/// still counts as set here, and only [`resolve`] discovers otherwise.
#[must_use]
pub fn is_set(file: Option<&Path>, inline: Option<&str>) -> bool {
	file.is_some() || inline.is_some_and(|inline| !inline.is_empty())
}

/// Resolves a configured secret from a file or an inline value.
///
/// A successfully read file takes precedence and is trimmed; an empty file
/// yields `None` instead of falling back inline. Read failures are logged and
/// permit the inline value to be used, while empty values are discarded.
#[must_use]
pub fn resolve(file: Option<&Path>, inline: Option<&str>, name: &str) -> Option<Secret> {
	let from_file = file.and_then(|path| {
		read_to_string(path)
			.inspect_err(|e| error!(%e, %name, "Failed to read secret file"))
			.ok()
	});

	from_file
		.as_deref()
		.map(str::trim)
		.or(inline)
		.filter(|secret| !secret.is_empty())
		.map(Secret::from)
}
