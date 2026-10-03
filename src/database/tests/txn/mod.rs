use serde::{Serialize, Serializer, ser::Error as SerError};
use tuwunel_core::Result;

use super::open_database;
use crate::{Txn, serialize_key, serialize_val};

struct Invalid;

impl Serialize for Invalid {
	fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
		Err(S::Error::custom("candidate codec failure"))
	}
}

#[tokio::test]
async fn fallible_transaction_barriers() -> Result {
	let db = open_database("fallible-txn").await?;
	let first = db.get("alias_roomid")?;
	let second = db.get("alias_userid")?;
	let key = b"accepted";
	let unsynced = b"unsynced";
	let cork = db.cork();

	Txn::insert_each([(first.as_ref(), key, b"a"), (second.as_ref(), key, b"b")])
		.try_execute()
		.expect("durable atomic write while corked");

	assert_eq!(first.get(key).await?.as_ref(), b"a");
	assert_eq!(second.get(key).await?.as_ref(), b"b");

	serialize_key(Invalid).expect_err("fallible key encoding");
	serialize_val(Invalid).expect_err("fallible value encoding");
	Txn::new(&db.engine)
		.try_execute()
		.expect("empty transaction succeeds");

	Txn::insert(first.as_ref(), [(unsynced, b"c")])
		.try_write()
		.expect("accepted write ahead of its barrier");

	assert_eq!(first.get(unsynced).await?.as_ref(), b"c");
	Txn::new(&db.engine)
		.try_write()
		.expect("empty unsynced transaction succeeds");

	drop(cork);
	db.engine.sync()?;

	Ok(())
}
