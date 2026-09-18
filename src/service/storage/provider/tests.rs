//! Tests for storage-provider payload chunking.
//!
//! The cases cover aligned, partial, empty, and maximum-sized payloads. They
//! verify chunk boundaries without requiring a live storage backend.

use object_store::PutPayload;

use super::chunked;

#[test]
fn chunked_splits_into_part_sized_chunks() {
	let payload = PutPayload::from(vec![0_u8; 25]);
	let chunks: Vec<PutPayload> = chunked(payload, 10).collect();

	assert_eq!(chunks.len(), 3);
	assert_eq!(chunks[0].content_length(), 10);
	assert_eq!(chunks[1].content_length(), 10);
	assert_eq!(chunks[2].content_length(), 5);
}

#[test]
fn chunked_aligned_size_yields_no_remainder() {
	let payload = PutPayload::from(vec![0_u8; 30]);
	let chunks: Vec<PutPayload> = chunked(payload, 10).collect();

	assert_eq!(chunks.len(), 3);
	assert!(chunks.iter().all(|c| c.content_length() == 10));
}

#[test]
fn chunked_smaller_than_part_size_yields_one() {
	let payload = PutPayload::from(vec![0_u8; 5]);
	let chunks: Vec<PutPayload> = chunked(payload, 10).collect();

	assert_eq!(chunks.len(), 1);
	assert_eq!(chunks[0].content_length(), 5);
}

#[test]
fn chunked_empty_payload_yields_nothing() {
	let payload = PutPayload::from(Vec::<u8>::new());

	assert!(chunked(payload, 10).next().is_none());
}

#[test]
fn chunked_usize_max_yields_one_part() {
	let payload = PutPayload::from(vec![0_u8; 1024]);
	let chunks: Vec<PutPayload> = chunked(payload, usize::MAX).collect();

	assert_eq!(chunks.len(), 1);
	assert_eq!(chunks[0].content_length(), 1024);
}
