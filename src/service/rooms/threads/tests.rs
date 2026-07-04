use tuwunel_core::{
	matrix::pdu::{PduCount, PduId, RawPduId},
	utils::u64_from_u8,
};

#[test]
fn backfilled_activity_key_sorts_before_normal() {
	let shortroomid = 0x0102_0304_0506_0708_u64;

	let normal: RawPduId = PduId { shortroomid, count: PduCount::Normal(1) }.into();

	let backfilled: RawPduId = PduId {
		shortroomid,
		count: PduCount::Backfilled(-5),
	}
	.into();

	assert_eq!(normal.shortroomid(), backfilled.shortroomid());
	assert!(backfilled.as_bytes() < normal.as_bytes());
}

#[test]
fn latest_count_value_round_trips() {
	let counts = [
		PduCount::Normal(1),
		PduCount::Normal(0x1112_1314_1516_1718),
		PduCount::Backfilled(0),
		PduCount::Backfilled(-42),
	];

	for count in counts {
		let read = PduCount::from_unsigned(u64_from_u8(&count.to_be_bytes()));

		assert_eq!(read, count);
	}
}
