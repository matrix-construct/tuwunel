#![cfg(test)]

use std::{fs::remove_dir_all, iter::once, sync::Arc};

use serde_json::{Value, json};
use tokio::{net::TcpListener, spawn, sync::mpsc::unbounded_channel};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	ruma::{DeviceId, UserId, device_id},
};
use tuwunel_service::{Services, users::DeviceListChange};

use self::stub::{Stub, parse, recv_txn, register, serve};

#[path = "appservice/stub.rs"]
mod stub;

const TO_DEVICE: &str = "de.sorunome.msc2409.to_device";
const DEVICE_LISTS: &str = "org.matrix.msc3202.device_lists";
const OTK_COUNT: &str = "org.matrix.msc3202.device_one_time_keys_count";
const FALLBACK: &str = "org.matrix.msc3202.device_unused_fallback_key_types";

/// Boots a server with a stub appservice listener and asserts the transactions
/// it receives: to-device delivery (MSC4203), the MSC3202 transaction
/// extensions gated on the registration opt-in, and stable-yet-distinct
/// transaction IDs.
#[test]
fn appservice_e2ee_transactions() -> Result {
	let db_path = Args::test_database_path("appservice-txn");

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_database_path(&db_path)
		.with_maintenance();

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = async_start(&server).await?;

		let outcome = run_cases(&services).await;

		server.server.shutdown()?;
		drop(services);

		async_run(&server).await?;
		async_stop(&server).await?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn run_cases(services: &Arc<Services>) -> Result {
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let url = format!("http://{}", listener.local_addr()?);

	let (tx, mut rx) = unbounded_channel();
	let stub = spawn(serve(listener, tx));

	let server_name = services.globals.server_name();
	let on = Stub {
		id: "voicebridge_on",
		url: &url,
		sender_localpart: "_von",
		users: "@_von_.*",
		msc3202: true,
	};

	let off = Stub {
		id: "voicebridge_off",
		sender_localpart: "_voff",
		users: "@_voff_.*",
		msc3202: false,
		..on
	};

	register(services, &on).await?;
	register(services, &off).await?;

	let on_ghost = UserId::parse_with_server_name("_von_ghost", server_name)?;
	let off_ghost = UserId::parse_with_server_name("_voff_ghost", server_name)?;
	let sender = UserId::parse("@alice:remote.example")?;
	let device = device_id!("GHOSTDEV");
	let content = json!({ "algorithm": "m.olm.v1.curve25519-aes-sha2", "ciphertext": {} });

	// The opted-in bridge gets the to-device event plus an MSC3202 one-time-key
	// count for the device (empty here: the drained-pool signal).
	send_to_device(services, &sender, &on_ghost, device, &content).await?;
	let (first_txn_id, body) = recv_txn(&mut rx).await?;
	let txn = parse(&body)?;
	assert_to_device(&txn, &on_ghost, device, &sender)?;
	assert_otk_entry(&txn, &on_ghost, device)?;

	// Gate isolation: an opted-out bridge receives the to-device event but none
	// of the three MSC3202 fields.
	send_to_device(services, &sender, &off_ghost, device, &content).await?;
	let (_, body) = recv_txn(&mut rx).await?;
	let txn = parse(&body)?;
	assert_to_device(&txn, &off_ghost, device, &sender)?;
	assert_no_msc3202(&txn)?;

	// A device-key change fans out a `device_lists.changed` marker.
	services
		.users
		.mark_device_key_update(&on_ghost, DeviceListChange::Resync)
		.await;

	let (_, body) = recv_txn(&mut rx).await?;
	let txn = parse(&body)?;
	assert_device_lists_changed(&txn, &on_ghost)?;

	// A byte-identical repeat still ships under a distinct transaction ID so the
	// bridge's txn-id dedup does not drop it.
	send_to_device(services, &sender, &on_ghost, device, &content).await?;
	let (second_txn_id, _) = recv_txn(&mut rx).await?;
	if first_txn_id == second_txn_id {
		return Err!("identical consecutive transactions shared txn id {first_txn_id}");
	}

	stub.abort();

	Ok(())
}

async fn send_to_device(
	services: &Services,
	sender: &UserId,
	target: &UserId,
	device: &DeviceId,
	content: &Value,
) -> Result {
	let count =
		services
			.users
			.add_to_device_event(sender, target, device, "m.room.encrypted", content);

	services
		.sending
		.send_to_device_appservices(
			sender,
			target,
			once((device, count)),
			"m.room.encrypted",
			content,
		)
		.await
}

fn assert_to_device(txn: &Value, target: &UserId, device: &DeviceId, sender: &UserId) -> Result {
	let event = txn
		.get(TO_DEVICE)
		.and_then(Value::as_array)
		.and_then(|events| events.first())
		.ok_or_else(|| err!("transaction had no {TO_DEVICE} entry"))?;

	let field = |name: &str| {
		event
			.get(name)
			.and_then(Value::as_str)
			.map(ToOwned::to_owned)
			.unwrap_or_default()
	};

	if field("type") != "m.room.encrypted"
		|| field("sender") != sender.as_str()
		|| field("to_user_id") != target.as_str()
		|| field("to_device_id") != device.as_str()
		|| event.get("content").is_none()
	{
		return Err!("unexpected to-device entry: {event}");
	}

	Ok(())
}

fn assert_otk_entry(txn: &Value, target: &UserId, device: &DeviceId) -> Result {
	txn.get(OTK_COUNT)
		.and_then(|counts| counts.get(target.as_str()))
		.and_then(|devices| devices.get(device.as_str()))
		.ok_or_else(|| {
			err!("transaction lacked an {OTK_COUNT} entry for the addressed device")
		})?;

	Ok(())
}

fn assert_no_msc3202(txn: &Value) -> Result {
	for field in [DEVICE_LISTS, OTK_COUNT, FALLBACK] {
		if txn.get(field).is_some() {
			return Err!("opted-out bridge received the gated field {field}");
		}
	}

	Ok(())
}

fn assert_device_lists_changed(txn: &Value, target: &UserId) -> Result {
	let changed = txn
		.get(DEVICE_LISTS)
		.and_then(|lists| lists.get("changed"))
		.and_then(Value::as_array)
		.ok_or_else(|| err!("transaction had no {DEVICE_LISTS}.changed array"))?;

	changed
		.iter()
		.any(|user| user.as_str() == Some(target.as_str()))
		.then_some(())
		.ok_or_else(|| err!("{DEVICE_LISTS}.changed did not contain the re-keyed user"))
}
