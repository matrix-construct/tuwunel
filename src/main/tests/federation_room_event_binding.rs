#![cfg(test)]

use std::{fs::remove_dir_all, net::TcpListener, time::Duration};

use futures::future::join;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err,
	ruma::{
		EventId, OwnedEventId, OwnedRoomId, RoomId, UserId,
		api::{
			OutgoingRequest, OutgoingRequestExt,
			federation::{
				authentication::{ServerSignatures, ServerSignaturesInput},
				authorization::get_event_authorization::v1::Request as EventAuthRequest,
				event::{
					get_room_state::v1::Request as StateRequest,
					get_room_state_ids::v1::Request as StateIdsRequest,
				},
			},
			path_builder::SinglePath,
		},
		events::StateEventType,
	},
};
use tuwunel_service::{Services, users::Register};

const ROUTES: [Route; 3] = [Route::State, Route::StateIds, Route::EventAuth];

#[derive(Clone, Copy, Debug)]
enum Route {
	State,
	StateIds,
	EventAuth,
}

#[test]
fn federation_routes_bind_events_to_rooms() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let db_path = Args::test_database_path("room-event-binding");
	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={db_path:?}"),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
	]);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user_id = UserId::parse_with_server_name("binding", services.globals.server_name())?;
	let token = "room-event-binding-regression-token";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("room-event-binding-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let room = create_room(services, base, token, None).await?;
	let event = state_event_id(services, &room).await?;
	let room_v12 = create_room(services, base, token, Some("12")).await?;
	let event_v12 = state_event_id(services, &room_v12).await?;
	let create_v12 = create_event_id(services, &room_v12).await?;
	let unknown = EventId::parse("$room-event-binding-unknown")?;

	for route in ROUTES {
		let response = route.get(services, base, &room, &event).await?;

		assert_eq!(response.0, 200, "{route:?}: {}", response.1);

		let foreign = route
			.get(services, base, &room, &event_v12)
			.await?;

		let unknown = route.get(services, base, &room, &unknown).await?;

		assert_eq!(foreign.0, 404);
		assert_eq!(unknown, foreign);

		let response_v12 = route
			.get(services, base, &room_v12, &event_v12)
			.await?;

		assert_eq!(response_v12.0, 200, "{route:?}: {}", response_v12.1);
	}

	let create_response = Route::EventAuth
		.get(services, base, &room_v12, &create_v12)
		.await?;

	assert_eq!(create_response.0, 200, "v12 create event: {}", create_response.1);

	Ok(())
}

impl Route {
	async fn get(
		self,
		services: &Services,
		base: &str,
		room_id: &RoomId,
		event_id: &EventId,
	) -> Result<(u16, String)> {
		match self {
			| Self::State => {
				let request = StateRequest::new(event_id.to_owned(), room_id.to_owned());

				send(services, base, request).await
			},
			| Self::StateIds => {
				let request = StateIdsRequest::new(event_id.to_owned(), room_id.to_owned());

				send(services, base, request).await
			},
			| Self::EventAuth => {
				let request = EventAuthRequest::new(room_id.to_owned(), event_id.to_owned());

				send(services, base, request).await
			},
		}
	}
}

async fn send<T>(services: &Services, base: &str, request: T) -> Result<(u16, String)>
where
	T: OutgoingRequest<Authentication = ServerSignatures, PathBuilder = SinglePath>,
{
	let server_name = services.globals.server_name().to_owned();
	let auth = ServerSignaturesInput::new(
		server_name.clone(),
		server_name,
		services.server_keys.keypair(),
	);
	let request = request.try_into_http_request::<Vec<u8>>(base, auth, ())?;
	let response = services
		.client
		.clients
		.default
		.execute(request.try_into()?)
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
}

async fn wait_until_ready(services: &Services, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");

	timeout(Duration::from_secs(10), async {
		loop {
			if services
				.client
				.clients
				.default
				.get(&url)
				.send()
				.await
				.is_ok()
			{
				break;
			}

			sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.map_err(|_| err!("server listener did not become ready"))?;

	Ok(())
}

async fn create_room(
	services: &Services,
	base: &str,
	token: &str,
	room_version: Option<&str>,
) -> Result<OwnedRoomId> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/createRoom"))
		.bearer_auth(token)
		.json(&json!({ "room_version": room_version }))
		.send()
		.await?
		.error_for_status()?
		.json::<Value>()
		.await?;

	let room_id = response
		.get("room_id")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("createRoom response omitted room_id"))?;

	Ok(room_id.try_into()?)
}

async fn state_event_id(services: &Services, room_id: &RoomId) -> Result<OwnedEventId> {
	services
		.state_accessor
		.room_state_get_id(room_id, &StateEventType::RoomGuestAccess, "")
		.await
}

async fn create_event_id(services: &Services, room_id: &RoomId) -> Result<OwnedEventId> {
	services
		.state_accessor
		.room_state_get_id(room_id, &StateEventType::RoomCreate, "")
		.await
}
