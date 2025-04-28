//! hmalloc allocator

#[global_allocator]
static HMALLOC: hardened_malloc_rs::HardenedMalloc = hardened_malloc_rs::HardenedMalloc;

pub fn trim<I: Into<Option<usize>>>(_: I) -> crate::Result {
	Ok(())
}

#[must_use]
//TODO: get usage
pub fn memory_usage() -> Option<String> {
	None
}

#[must_use]
pub fn memory_stats(_opts: &str) -> Option<String> {
	Some("Extended statistics are not available from hardened_malloc.".to_owned())
}
