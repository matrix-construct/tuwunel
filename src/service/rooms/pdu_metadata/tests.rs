use tuwunel_core::{matrix::PduCount, utils::u64_from_u8};

use super::typed_relations::{CHILD_COUNT_OFFSET, KEY_LEN, PREFIX_LEN, Tag, key, prefix};

#[test]
fn typed_relation_key_round_trips() {
	let shortroomid = 0x0102_0304_0506_0708_u64;
	let parent = PduCount::Normal(0x1112_1314_1516_1718);
	let child_ts = 0x2122_2324_2526_2728_u64;
	let child = PduCount::Normal(0x3132_3334_3536_3738);

	let key = key(shortroomid, parent, Tag::Replace, child_ts, child);

	assert_eq!(key.len(), KEY_LEN);
	assert_eq!(&key[..8], &shortroomid.to_be_bytes());
	assert_eq!(&key[8..16], &parent.to_be_bytes());
	assert_eq!(key[16], u8::from(Tag::Replace));
	assert_eq!(&key[17..25], &child_ts.to_be_bytes());

	let read_child = PduCount::from_unsigned(u64_from_u8(&key[CHILD_COUNT_OFFSET..KEY_LEN]));

	assert_eq!(read_child, child);

	let prefix = prefix(shortroomid, parent, Tag::Replace);

	assert_eq!(prefix.len(), PREFIX_LEN);
	assert!(key.starts_with(&prefix));
}

#[test]
fn rel_tag_wire_bytes() {
	assert_eq!(u8::from(Tag::Replace), 0x01);
	assert_eq!(u8::from(Tag::Reference), 0x02);
}
