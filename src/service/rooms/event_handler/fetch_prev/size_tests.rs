use serde_json::value::RawValue;
use tuwunel_matrix::pdu::MAX_PDU_BYTES;

use super::parse_missing_event;

#[test]
fn missing_event_size_precedes_canonical_decoding() {
	let padding = " ".repeat(4 * MAX_PDU_BYTES);
	let raw = RawValue::from_string(format!("[{padding}]")).expect("valid JSON array");
	let error = parse_missing_event(&raw).expect_err("not a PDU");
	assert!(error.to_string().contains("larger than maximum"), "{error}");
}
