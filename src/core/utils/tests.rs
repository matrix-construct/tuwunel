use crate::utils;

#[test]
fn increment_none() {
	let bytes: [u8; 8] = utils::increment(None);
	let res = u64::from_be_bytes(bytes);
	assert_eq!(res, 1);
}

#[test]
fn increment_fault() {
	let start: u8 = 127;
	let bytes: [u8; 1] = start.to_be_bytes();
	let bytes: [u8; 8] = utils::increment(Some(&bytes));
	let res = u64::from_be_bytes(bytes);
	assert_eq!(res, 1);
}

#[test]
fn increment_norm() {
	let start: u64 = 1_234_567;
	let bytes: [u8; 8] = start.to_be_bytes();
	let bytes: [u8; 8] = utils::increment(Some(&bytes));
	let res = u64::from_be_bytes(bytes);
	assert_eq!(res, 1_234_568);
}

#[test]
fn increment_wrap() {
	let start = u64::MAX;
	let bytes: [u8; 8] = start.to_be_bytes();
	let bytes: [u8; 8] = utils::increment(Some(&bytes));
	let res = u64::from_be_bytes(bytes);
	assert_eq!(res, 0);
}

#[test]
fn checked_add() {
	use crate::checked;

	let a = 1234;
	let res = checked!(a + 1).unwrap();
	assert_eq!(res, 1235);
}

#[test]
#[should_panic(expected = "overflow")]
fn checked_add_overflow() {
	use crate::checked;

	let a = u64::MAX;
	let res = checked!(a + 1).expect("overflow");
	assert_eq!(res, 0);
}

#[tokio::test]
async fn mutex_map_cleanup() {
	use crate::utils::MutexMap;

	let map = MutexMap::<String, ()>::new();

	let lock = map.lock("foo").await;
	assert!(!map.is_empty(), "map must not be empty");

	drop(lock);
	assert!(map.is_empty(), "map must be empty");
}

#[tokio::test]
async fn mutex_map_contend() {
	use std::sync::Arc;

	use tokio::sync::Barrier;

	use crate::utils::MutexMap;

	let map = Arc::new(MutexMap::<String, ()>::new());
	let seq = Arc::new([Barrier::new(2), Barrier::new(2)]);
	let str = "foo".to_owned();

	let seq_ = seq.clone();
	let map_ = map.clone();
	let str_ = str.clone();
	let join_a = tokio::spawn(async move {
		let _lock = map_.lock(&str_).await;
		assert!(!map_.is_empty(), "A0 must not be empty");
		seq_[0].wait().await;
		assert!(map_.contains(&str_), "A1 must contain key");
	});

	let seq_ = seq.clone();
	let map_ = map.clone();
	let str_ = str.clone();
	let join_b = tokio::spawn(async move {
		let _lock = map_.lock(&str_).await;
		assert!(!map_.is_empty(), "B0 must not be empty");
		seq_[1].wait().await;
		assert!(map_.contains(&str_), "B1 must contain key");
	});

	seq[0].wait().await;
	assert!(map.contains(&str), "Must contain key");
	seq[1].wait().await;

	tokio::try_join!(join_b, join_a).expect("joined");
	assert!(map.is_empty(), "Must be empty");
}

#[test]
fn set_intersection_none() {
	use utils::set::intersection;

	let a: [&str; 0] = [];
	let b: [&str; 0] = [];
	let i = [a.iter(), b.iter()];
	let r = intersection(i.into_iter());
	assert_eq!(r.count(), 0);

	let a: [&str; 0] = [];
	let b = ["abc", "def"];
	let i = [a.iter(), b.iter()];
	let r = intersection(i.into_iter());
	assert_eq!(r.count(), 0);
	let i = [b.iter(), a.iter()];
	let r = intersection(i.into_iter());
	assert_eq!(r.count(), 0);
	let i = [a.iter()];
	let r = intersection(i.into_iter());
	assert_eq!(r.count(), 0);

	let a = ["foo", "bar", "baz"];
	let b = ["def", "hij", "klm", "nop"];
	let i = [a.iter(), b.iter()];
	let r = intersection(i.into_iter());
	assert_eq!(r.count(), 0);
}

#[test]
#[expect(clippy::iter_on_single_items, clippy::many_single_char_names)]
fn set_intersection_all() {
	use utils::set::intersection;

	let a = ["foo"];
	let b = ["foo"];
	let i = [a.iter(), b.iter()];
	let r = intersection(i.into_iter());
	assert!(r.eq(["foo"].iter()));

	let a = ["foo", "bar"];
	let b = ["bar", "foo"];
	let i = [a.iter(), b.iter()];
	let r = intersection(i.into_iter());
	assert!(r.eq(["foo", "bar"].iter()));
	let i = [b.iter()];
	let r = intersection(i.into_iter());
	assert!(r.eq(["bar", "foo"].iter()));

	let a = ["foo", "bar", "baz"];
	let b = ["baz", "foo", "bar"];
	let c = ["bar", "baz", "foo"];
	let i = [a.iter(), b.iter(), c.iter()];
	let r = intersection(i.into_iter());
	assert!(r.eq(["foo", "bar", "baz"].iter()));
}

#[test]
#[expect(clippy::iter_on_single_items, clippy::many_single_char_names)]
fn set_intersection_some() {
	use utils::set::intersection;

	let a = ["foo"];
	let b = ["bar", "foo"];
	let i = [a.iter(), b.iter()];
	let r = intersection(i.into_iter());
	assert!(r.eq(["foo"].iter()));
	let i = [b.iter(), a.iter()];
	let r = intersection(i.into_iter());
	assert!(r.eq(["foo"].iter()));

	let a = ["abcdef", "foo", "hijkl", "abc"];
	let b = ["hij", "bar", "baz", "abc", "foo"];
	let c = ["abc", "xyz", "foo", "ghi"];
	let i = [a.iter(), b.iter(), c.iter()];
	let r = intersection(i.into_iter());
	assert!(r.eq(["foo", "abc"].iter()));
}

#[test]
#[expect(clippy::iter_on_single_items, clippy::many_single_char_names)]
fn set_intersection_sorted_some() {
	use utils::set::intersection_sorted;

	let a = ["bar"];
	let b = ["bar", "foo"];
	let i = [a.iter(), b.iter()];
	let r = intersection_sorted(i.into_iter());
	assert!(r.eq(["bar"].iter()));
	let i = [b.iter(), a.iter()];
	let r = intersection_sorted(i.into_iter());
	assert!(r.eq(["bar"].iter()));

	let a = ["aaa", "ccc", "eee", "ggg"];
	let b = ["aaa", "bbb", "ccc", "ddd", "eee"];
	let c = ["bbb", "ccc", "eee", "fff"];
	let i = [a.iter(), b.iter(), c.iter()];
	let r = intersection_sorted(i.into_iter());
	assert!(r.eq(["ccc", "eee"].iter()));
}

#[test]
#[expect(clippy::iter_on_single_items, clippy::many_single_char_names)]
fn set_intersection_sorted_all() {
	use utils::set::intersection_sorted;

	let a = ["foo"];
	let b = ["foo"];
	let i = [a.iter(), b.iter()];
	let r = intersection_sorted(i.into_iter());
	assert!(r.eq(["foo"].iter()));

	let a = ["bar", "foo"];
	let b = ["bar", "foo"];
	let i = [a.iter(), b.iter()];
	let r = intersection_sorted(i.into_iter());
	assert!(r.eq(["bar", "foo"].iter()));
	let i = [b.iter()];
	let r = intersection_sorted(i.into_iter());
	assert!(r.eq(["bar", "foo"].iter()));

	let a = ["bar", "baz", "foo"];
	let b = ["bar", "baz", "foo"];
	let c = ["bar", "baz", "foo"];
	let i = [a.iter(), b.iter(), c.iter()];
	let r = intersection_sorted(i.into_iter());
	assert!(r.eq(["bar", "baz", "foo"].iter()));
}

#[tokio::test]
async fn set_intersection_sorted_stream2() {
	use futures::StreamExt;
	use utils::{IterStream, set::intersection_sorted_stream2};

	let a = ["bar"];
	let b = ["bar", "foo"];
	let r = intersection_sorted_stream2(a.iter().stream(), b.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	assert!(r.eq(&["bar"]));

	let r = intersection_sorted_stream2(b.iter().stream(), a.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	assert!(r.eq(&["bar"]));

	let a = ["aaa", "ccc", "xxx", "yyy"];
	let b = ["hhh", "iii", "jjj", "zzz"];
	let r = intersection_sorted_stream2(a.iter().stream(), b.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	assert!(r.is_empty());

	let a = ["aaa", "ccc", "eee", "ggg"];
	let b = ["aaa", "bbb", "ccc", "ddd", "eee"];
	let r = intersection_sorted_stream2(a.iter().stream(), b.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	assert!(r.eq(&["aaa", "ccc", "eee"]));

	let a = ["aaa", "ccc", "eee", "ggg", "hhh", "iii"];
	let b = ["bbb", "ccc", "ddd", "fff", "ggg", "iii"];
	let r = intersection_sorted_stream2(a.iter().stream(), b.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	assert!(r.eq(&["ccc", "ggg", "iii"]));
}

#[tokio::test]
async fn set_difference_sorted_stream2() {
	use futures::StreamExt;
	use utils::{IterStream, set::difference_sorted_stream2};

	let a = ["bar", "foo"];
	let b = ["bar"];
	let r = difference_sorted_stream2(a.iter().stream(), b.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	println!("{r:?}");
	assert!(r.eq(&["foo"]));

	let r = difference_sorted_stream2(b.iter().stream(), a.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	println!("{r:?}");
	assert!(r.is_empty());

	let a = ["aaa", "ccc", "xxx", "yyy"];
	let b = ["hhh", "iii", "jjj", "zzz"];
	let r = difference_sorted_stream2(a.iter().stream(), b.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	println!("{r:?}");
	assert!(r.eq(&["aaa", "ccc", "xxx", "yyy"]));

	let a = ["aaa", "ccc", "eee", "ggg"];
	let b = ["aaa", "bbb", "ccc", "ddd", "eee"];
	let r = difference_sorted_stream2(a.iter().stream(), b.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	println!("{r:?}");
	assert!(r.eq(&["ggg"]));

	let a = ["aaa", "ccc", "eee", "ggg", "hhh", "iii"];
	let b = ["bbb", "ccc", "ddd", "fff", "ggg", "iii"];
	let r = difference_sorted_stream2(a.iter().stream(), b.iter().stream())
		.collect::<Vec<&str>>()
		.await;
	println!("{r:?}");
	assert!(r.eq(&["aaa", "eee", "hhh"]));
}
