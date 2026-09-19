#![cfg(test)]

use std::net::TcpListener;

use futures::{TryStreamExt, future::join};
use reqwest::Method;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{UserId, device_id},
};
use tuwunel_service::{
	Services,
	users::DeviceListChange::{CrossSigning, Deleted, Device, Resync},
};

use self::client::{Client, field, wait_until_ready};

#[expect(
	dead_code,
	reason = "shared client helpers serve sibling integration tests"
)]
mod client;

#[test]
fn records_follow_the_device_lifecycle() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("allow_registration=true")
		.with_option(
			"yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse=true",
		);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run?;
		outcome
	})
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let client = Client { services, base, token: "" };
	let registration = json!({"username": "record", "password": "record-password",
		"device_id": "RECORD", "auth": {"type": "m.login.dummy"}});

	let session = client.post("register", &registration).await?;
	let user = UserId::parse(field(&session, "user_id")?)?;
	let client = Client {
		services,
		base,
		token: field(&session, "access_token")?,
	};

	let upload = json!({"device_keys": {"user_id": user, "device_id": "RECORD",
		"algorithms": [], "keys": {}, "signatures": {}}});

	client.post("keys/upload", &upload).await?;
	request(&client, Method::PUT, &json!({"display_name": "renamed"})).await?;

	let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
	let signing = json!({"master_key": {"user_id": user, "usage": ["master"],
		"keys": {format!("ed25519:{key}"): key}}});

	client
		.post("keys/device_signing/upload", &signing)
		.await?;

	let version = services
		.users
		.get_devicelist_version(&user)
		.await?;

	assert_eq!(version, 3);

	let auth = json!({"auth": {"type": "m.login.password",
		"identifier": {"type": "m.id.user", "user": user}, "password": "record-password"}});

	request(&client, Method::DELETE, &auth).await?;

	let prefix = (&user,);
	let records: Vec<_> = services.db["keychangeid_userid"]
		.keys_prefix(&prefix)
		.map_ok(|(_, count): (&UserId, u64)| count)
		.and_then(async |count| services.users.device_list_change(count).await)
		.map_ok(|record| (record.change, record.stream_id))
		.try_collect()
		.await?;

	let device = || device_id!("RECORD").to_owned();

	assert_eq!(records, [
		(Device(device()), 1),
		(Device(device()), 2),
		(Device(device()), 3),
		(CrossSigning, 3),
		(Deleted(device()), 4)
	]);

	let version = services
		.users
		.get_devicelist_version(&user)
		.await?;

	assert_eq!(version, 4);

	let absent = services.users.device_list_change(u64::MAX).await;

	assert!(absent.unwrap_err().is_not_found());
	services.db["keychangeid_devicechange"].put(u64::MAX, (u8::MAX, 7_u64, ""));

	let unknown = services
		.users
		.device_list_change(u64::MAX)
		.await?;

	assert_eq!((unknown.change, unknown.stream_id), (Resync, 7));

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn request(client: &Client<'_>, method: Method, body: &Value) -> Result {
	client
		.services
		.client
		.clients
		.default
		.request(method, client.url("devices/RECORD"))
		.bearer_auth(client.token)
		.json(body)
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}
