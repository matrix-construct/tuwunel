//! Default allocator with no special features

/// Always returns Ok
pub fn trim<I: Into<Option<usize>>>(_: I) -> crate::Result {
	Ok(())
}

/// Always returns None
#[must_use]
pub fn memory_stats(_opts: &str) -> Option<String> {
	None
}

/// Always returns None
#[must_use]
pub fn memory_usage() -> Option<String> {
	None
}
