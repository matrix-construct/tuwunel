use ruma::{CanonicalJsonObject, server_name};
use serde_json::{Value, from_value, json, to_value};

use super::with_server_signatures;

#[test]
fn added_signatures_join_the_held_ones() {
	let pdu = object(json!({
		"signatures": {
			"origin.example": { "ed25519:a": "origin" },
			"invitee.example": { "ed25519:policy_server": "policy" },
		},
	}));

	let merged = signed(pdu, json!({ "ed25519:b": "invitee" }));
	let expected = json!({
		"origin.example": { "ed25519:a": "origin" },
		"invitee.example": { "ed25519:policy_server": "policy", "ed25519:b": "invitee" },
	});

	assert_eq!(merged["signatures"], expected);
}

#[test]
fn a_held_signature_wins_its_key() {
	let pdu = object(json!({
		"signatures": { "invitee.example": { "ed25519:policy_server": "policy" } },
	}));

	let added = json!({ "ed25519:policy_server": "answer", "ed25519:b": "invitee" });
	let merged = signed(pdu, added);
	let expected = json!({ "ed25519:policy_server": "policy", "ed25519:b": "invitee" });

	assert_eq!(merged["signatures"]["invitee.example"], expected);
}

#[test]
fn a_new_server_gets_the_added_set() {
	let pdu = object(json!({ "signatures": { "origin.example": { "ed25519:a": "origin" } } }));
	let merged = signed(pdu, json!({ "ed25519:b": "invitee" }));

	assert_eq!(merged["signatures"]["invitee.example"], json!({ "ed25519:b": "invitee" }));
	assert_eq!(merged["signatures"]["origin.example"], json!({ "ed25519:a": "origin" }));
}

fn object(value: Value) -> CanonicalJsonObject { from_value(value).expect("canonical object") }

fn signed(pdu: CanonicalJsonObject, added: Value) -> Value {
	let merged = with_server_signatures(pdu, server_name!("invitee.example"), object(added));

	to_value(merged).expect("serializable object")
}
