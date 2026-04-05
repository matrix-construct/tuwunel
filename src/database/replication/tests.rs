#![cfg(test)]

use super::*;

#[test]
fn heartbeat_round_trip() {
	let frame = WalFrame::heartbeat(12345);
	let encoded = frame.encode_to_vec().unwrap();
	let (decoded, remaining) = WalFrame::decode(&encoded).unwrap();
	let consumed = encoded.len() - remaining.len();
	assert_eq!(consumed, encoded.len());
	assert_eq!(decoded.kind(), FrameKind::HeartBeat);
	assert_eq!(decoded.sequence, 12345);
	assert_eq!(decoded.count, 0);
	assert!(decoded.batch_data.is_empty());
	// Heartbeat does not advance resume cursor
	assert_eq!(decoded.next_resume_seq(), 12345);
}

#[test]
fn data_frame_round_trip() {
	let data = b"test writebatch payload bytes".to_vec();
	let frame = WalFrame::data(1000, 50, data.clone());
	let encoded = frame.encode_to_vec().unwrap();
	let (decoded, remaining) = WalFrame::decode(&encoded).unwrap();
	let consumed = encoded.len() - remaining.len();
	assert_eq!(consumed, encoded.len());
	assert_eq!(decoded.kind(), FrameKind::Data);
	assert_eq!(decoded.sequence, 1000);
	assert_eq!(decoded.count, 50);
	assert_eq!(decoded.next_resume_seq(), 1050);
	assert_eq!(decoded.batch_data, data);
}

#[test]
#[should_panic = "expected sequence advance"]
fn data_frame_zero_count_assert() {
	// A batch with count=0 should not advance the cursor beyond sequence.
	let frame = WalFrame::data(500, 0, b"payload".to_vec());
	assert_eq!(frame.next_resume_seq(), 500);
}

#[test]
fn data_frame_zero_count() {
	// A batch with count=0 should not advance the cursor beyond sequence.
	let frame = WalFrame::data(500, 0, Vec::new());
	assert_eq!(frame.next_resume_seq(), 500);
}

#[test]
fn truncated_body_rejected() {
	let frame = WalFrame::data(1, 1, b"hello world test".to_vec());
	let mut encoded = frame.encode_to_vec().unwrap();
	encoded.truncate(encoded.len() - 3);
	WalFrame::decode(&encoded).unwrap_err();
}

#[test]
fn multiple_frames_in_buffer() {
	let f1 = WalFrame::data(100, 5, b"batch one".to_vec());
	let f2 = WalFrame::heartbeat(105);
	let f3 = WalFrame::data(105, 3, b"batch two".to_vec());
	let mut buf = f1.encode_to_vec().unwrap();
	buf.extend_from_slice(&f2.encode_to_vec().unwrap());
	buf.extend_from_slice(&f3.encode_to_vec().unwrap());

	let (d1, c1) = WalFrame::decode(&buf).unwrap();
	assert!(!c1.is_empty());
	let (d2, c2) = WalFrame::decode(c1).unwrap();
	assert!(!c2.is_empty());
	let (d3, c3) = WalFrame::decode(c2).unwrap();
	assert_eq!(c3.len(), 0);

	assert_eq!(d1.sequence, 100);
	assert_eq!(d2.kind(), FrameKind::HeartBeat);
	assert_eq!(d3.sequence, 105);
	assert_eq!(c1.len() + c2.len() + c3.len(), buf.len());
}

#[test]
fn batch_count_from_bytes_valid() {
	let mut fake = vec![0_u8; 16];
	fake[8..12].copy_from_slice(&7_u32.to_le_bytes());
	assert_eq!(batch_count_from_bytes(&fake), 7);
}

#[test]
fn batch_count_from_bytes_too_short() {
	assert_eq!(batch_count_from_bytes(&[0_u8; 5]), 0);
	assert_eq!(batch_count_from_bytes(&[]), 0);
}
