use std::{pin::pin, time::Duration};

use futures::{StreamExt, future::join};
use ruma::user_id;
use serde_json::{Value as JsonValue, json};
use tokio::time::timeout;
use tuwunel_core::{Result, config::Figment};

use crate::test_utils::fixture;

const WATCH_WINDOW: Duration = Duration::from_millis(500);

const KIND: &str = "org.example.revision";

/// Two updates of one type waiting on their user's lock leave one revision,
/// the last write's, which the pointer names.
///
/// Each update reads the pointer under the lock, so it replaces the revision
/// the other stored. Neither holds a sequence number while it waits, so the
/// sync frontier is not held behind the lock.
#[tokio::test]
async fn concurrent_updates_keep_one_revision() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let account_data = &fixture.services.account_data;
	let user_id = user_id!("@writer:localhost");
	let revision = |n: u8| json!({ "type": KIND, "content": { "n": n } });
	let (initial, one, two) = (revision(0), revision(1), revision(2));

	account_data
		.update(None, user_id, KIND.into(), &initial)
		.await?;

	let updating = account_data.updating.lock(user_id).await;
	let mut first = pin!(account_data.update(None, user_id, KIND.into(), &one));
	let mut second = pin!(account_data.update(None, user_id, KIND.into(), &two));
	let (first_waited, second_waited) =
		join(timeout(WATCH_WINDOW, first.as_mut()), timeout(WATCH_WINDOW, second.as_mut())).await;

	assert!(
		first_waited.is_err() && second_waited.is_err(),
		"an update finished while its user's lock was held"
	);

	let frontier = timeout(WATCH_WINDOW, fixture.services.globals.wait_pending()).await;

	assert!(
		matches!(frontier, Ok(Ok(_))),
		"an update waiting on the lock holds a sequence number"
	);

	drop(updating);
	first.await?;
	second.await?;

	let revisions = account_data
		.changes_since(None, user_id, 0, None)
		.count()
		.await;

	assert_eq!(revisions, 1, "a superseded revision is left in the log");

	let stored: JsonValue = account_data
		.get_global(user_id, KIND.into())
		.await?;

	assert_eq!(stored, two);

	Ok(())
}
