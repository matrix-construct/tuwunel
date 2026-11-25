use serde_json::{json, to_vec};

use super::{AuthFetchBudget, CanonicalJsonObject, MAX_PDU_BYTES, parse_fetched_pdu};

#[test]
fn fetched_pdu_size_excludes_unsigned() {
	// A state event served with the previous content under `unsigned`: over
	// the limit as served, within it once `unsigned` is removed.
	let content = json!({ "pad": "x".repeat(40_000) });
	let bytes = to_vec(&json!({
		"type": "m.room.power_levels",
		"content": content,
		"unsigned": { "prev_content": content },
	}))
	.expect("serializes");
	assert!(bytes.len() > MAX_PDU_BYTES);

	let (_, pdu_json) = parse_fetched_pdu(&bytes).expect("kept within the size limit");
	let pdu: CanonicalJsonObject = serde_json::from_slice(&pdu_json).expect("parses");
	assert!(!pdu.contains_key("unsigned"), "unsigned is not kept");
}

#[test]
fn fetched_pdu_over_the_size_limit_is_rejected() {
	let bytes = to_vec(&json!({
		"type": "m.room.message",
		"content": { "pad": "x".repeat(MAX_PDU_BYTES) },
	}))
	.expect("serializes");

	assert!(parse_fetched_pdu(&bytes).is_none());
}

#[test]
fn auth_chain_fetch_budget() {
	let budget = AuthFetchBudget::new(2);
	budget
		.check_count(1)
		.expect("second event fits in the walk");
	budget
		.check_count(2)
		.expect_err("third event exceeds the walk limit");

	budget
		.charge(MAX_PDU_BYTES)
		.expect("first root fits");
	budget
		.charge(MAX_PDU_BYTES)
		.expect("second root uses the last byte");
	budget
		.charge(1)
		.expect_err("root walks share the same budget");
}
