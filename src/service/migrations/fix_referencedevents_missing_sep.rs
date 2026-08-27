use std::{cmp::max, str};

use futures::StreamExt;
use tuwunel_core::{
	Result, debug, info,
	utils::{ReadyExt, stream::TryExpect},
	warn,
};
use tuwunel_database::SEP;

use crate::Services;

pub(super) async fn fix_referencedevents_missing_sep(services: &Services) -> Result {
	warn!("Fixing missing record separator between room_id and event_id in referencedevents");

	let db = &services.db;
	let cork = db.cork_and_sync();

	let referencedevents = db["referencedevents"].clone();

	let totals: (usize, usize) = (0, 0);
	let (total, fixed) = referencedevents
		.raw_stream()
		.expect_ok()
		.enumerate()
		.ready_fold(totals, |mut a, (i, (key, val))| {
			debug_assert!(val.is_empty(), "expected no value");

			services.server.progress.advance();

			let has_sep = key.contains(&SEP);

			if !has_sep {
				let key_str = str::from_utf8(key).expect("key not utf-8");
				let room_id_len = key_str.find('$').expect("missing '$' in key");
				let (room_id, event_id) = key_str.split_at(room_id_len);

				debug!(?a, "fixing {room_id}, {event_id}");

				let new_key = (room_id, event_id);
				referencedevents.put_raw(new_key, val);
				referencedevents.remove(key);
			}

			a.0 = max(i, a.0);
			a.1 = a.1.saturating_add((!has_sep).into());
			a
		})
		.await;

	drop(cork);
	info!(?total, ?fixed, "Fixed missing record separators in 'referencedevents'.");

	db["global"].insert(b"fix_referencedevents_missing_sep", []);
	referencedevents.sort()
}
