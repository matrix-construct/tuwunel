use super::{
	Destination, EduBuf, SendingEvent, TAG_DEVICE_LIST_CHANGED, TAG_TO_DEVICE,
	data::parse_servercurrentevent,
};

/// `RawId::NORMAL_LEN`: a `ShortRoomId` (u64) plus a count (u64).
const NORMAL_PDU_LEN: usize = 16;

fn appservice_prefix() -> Vec<u8> { Destination::Appservice("as1".to_owned()).get_prefix() }

/// A queued appservice row key for any count-keyed variant (everything but
/// `Pdu`, whose id lives in the key).
fn count_key() -> Vec<u8> {
	let mut key = appservice_prefix();
	key.extend_from_slice(&7_u64.to_be_bytes());

	key
}

fn tagged(tag: u8, body: &[u8]) -> EduBuf {
	let mut buf = EduBuf::new();
	buf.push(tag);
	buf.extend_from_slice(&3_u64.to_be_bytes());
	buf.extend_from_slice(body);

	buf
}

#[test]
fn appservice_to_device_round_trips() {
	let event = SendingEvent::ToDevice(tagged(TAG_TO_DEVICE, br#"{"type":"m.room.encrypted"}"#));
	let (dest, decoded) = parse_servercurrentevent(&count_key(), event.value_bytes())
		.expect("appservice to-device row decodes");

	assert!(matches!(dest, Destination::Appservice(_)));
	assert_eq!(decoded, event);
}

#[test]
fn appservice_device_list_round_trips() {
	let event = SendingEvent::DeviceListChanged(tagged(
		TAG_DEVICE_LIST_CHANGED,
		b"@ghost:remote.example",
	));
	let (_, decoded) = parse_servercurrentevent(&count_key(), event.value_bytes())
		.expect("appservice device-list row decodes");

	assert_eq!(decoded, event);
}

#[test]
fn appservice_legacy_edu_decodes_as_edu() {
	let event = SendingEvent::Edu(EduBuf::from_slice(br#"{"edu":"json"}"#));
	let (_, decoded) = parse_servercurrentevent(&count_key(), event.value_bytes())
		.expect("legacy edu row decodes");

	assert_eq!(decoded, event);
}

#[test]
fn appservice_empty_value_decodes_as_pdu() {
	let mut key = appservice_prefix();
	key.extend_from_slice(&[0x11; NORMAL_PDU_LEN]);

	let (_, decoded) = parse_servercurrentevent(&key, &[]).expect("empty appservice row decodes");

	assert!(matches!(decoded, SendingEvent::Pdu(_)));
	assert!(decoded.value_bytes().is_empty());
}

#[test]
fn appservice_unknown_tag_decodes_as_edu() {
	let (_, decoded) = parse_servercurrentevent(&count_key(), &[0x03, 0xAA, 0xBB])
		.expect("unknown-tag appservice row decodes");

	assert!(matches!(decoded, SendingEvent::Edu(_)));
}

#[test]
fn federation_branch_ignores_tag_bytes() {
	let mut key =
		Destination::Federation("remote.example".try_into().expect("server name")).get_prefix();
	key.extend_from_slice(&[0x11; NORMAL_PDU_LEN]);

	let (dest, event) = parse_servercurrentevent(&key, &[]).expect("federation pdu row decodes");
	assert!(matches!(dest, Destination::Federation(_)));
	assert!(matches!(event, SendingEvent::Pdu(_)));

	let (_, event) = parse_servercurrentevent(&key, &[TAG_TO_DEVICE, 0x00])
		.expect("federation non-empty row decodes");
	assert!(matches!(event, SendingEvent::Edu(_)));
}

#[test]
fn push_branch_ignores_tag_bytes() {
	let mut key =
		Destination::Push("@u:remote.example".try_into().expect("user id"), "pushkey".to_owned())
			.get_prefix();
	key.extend_from_slice(&[0x22; NORMAL_PDU_LEN]);

	let (dest, event) = parse_servercurrentevent(&key, &[]).expect("push pdu row decodes");
	assert!(matches!(dest, Destination::Push(..)));
	assert!(matches!(event, SendingEvent::Pdu(_)));

	let (_, event) = parse_servercurrentevent(&key, &[TAG_TO_DEVICE, 0x00])
		.expect("push non-empty row decodes");
	assert!(matches!(event, SendingEvent::Edu(_)));
}
