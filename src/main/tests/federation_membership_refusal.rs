#![cfg(test)]

use std::{
	env::temp_dir, fs::remove_dir_all, net::TcpListener, path::PathBuf,
	process::id as process_id, time::Duration,
};

use futures::future::join;
use serde_json::{
	Value, json,
	value::{RawValue as RawJsonValue, to_raw_value},
};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err,
	pdu::PduBuilder,
	ruma::{
		OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
		api::{
			OutgoingRequest, OutgoingRequestExt,
			federation::{
				authentication::{ServerSignatures, ServerSignaturesInput},
				membership::{
					create_join_event::v2::Request as JoinRequest,
					create_knock_event::v1::Request as KnockRequest,
					create_leave_event::v2::Request as LeaveRequest,
				},
			},
			path_builder::SinglePath,
		},
		events::room::{
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
		},
	},
};
use tuwunel_service::{Services, membership::Join, users::Register};

struct DatabasePath(PathBuf);

struct MembershipPdu {
	event_id: OwnedEventId,
	pdu: Box<RawJsonValue>,
}

#[derive(Clone, Copy, Debug)]
enum Route {
	Join,
	Leave,
	Knock,
}

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn membership_nonacceptance_is_forbidden() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let path = temp_dir()
		.join("tuwunel")
		.join(format!("membership-refusal-{}", process_id()));

	let db_path = DatabasePath(path);

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={:?}", db_path.0),
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
	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let admin = register(services, "membership-admin").await?;
	let token = "membership-refusal-regression-token";

	services
		.users
		.create_device(&admin, None, (Some(token), None), None, None, None)
		.await?;

	let room_id = create_room(services, base, token).await?;

	let joining = register(services, "joining-refusal").await?;
	let join_event = prepare(services, &joining, &room_id, MembershipState::Join).await?;

	assert_malformed(services, base, Route::Join, &room_id, &join_event).await?;
	ban(services, &admin, &joining, &room_id).await?;
	assert_refused(services, base, Route::Join, &room_id, &join_event).await?;

	let leaving = register(services, "leaving-refusal").await?;

	join_room(services, &leaving, &room_id).await?;
	let leave_event = prepare(services, &leaving, &room_id, MembershipState::Leave).await?;

	assert_malformed(services, base, Route::Leave, &room_id, &leave_event).await?;
	ban(services, &admin, &leaving, &room_id).await?;
	assert_refused(services, base, Route::Leave, &room_id, &leave_event).await?;

	set_knock_rule(services, &admin, &room_id).await?;

	let knocking = register(services, "knocking-refusal").await?;
	let knock_event = prepare(services, &knocking, &room_id, MembershipState::Knock).await?;

	assert_malformed(services, base, Route::Knock, &room_id, &knock_event).await?;
	ban(services, &admin, &knocking, &room_id).await?;
	assert_refused(services, base, Route::Knock, &room_id, &knock_event).await?;

	Ok(())
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

async fn register(services: &Services, localpart: &str) -> Result<OwnedUserId> {
	let user_id = UserId::parse_with_server_name(localpart, services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("membership-refusal-password"),
			..Default::default()
		})
		.await?;

	Ok(user_id)
}

async fn create_room(services: &Services, base: &str, token: &str) -> Result<OwnedRoomId> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/createRoom"))
		.bearer_auth(token)
		.json(&json!({ "preset": "public_chat" }))
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

async fn prepare(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	membership: MembershipState,
) -> Result<MembershipPdu> {
	let room_version = services.state.get_room_version(room_id).await?;
	let content = RoomMemberEventContent::new(membership);
	let builder = PduBuilder::state(user_id.to_string(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;
	let (event, json) = services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await?;

	drop(state_lock);

	let pdu = services
		.federation
		.format_pdu_into(json, Some(&room_version))
		.await;

	Ok(MembershipPdu { event_id: event.event_id, pdu })
}

async fn join_room(services: &Services, user_id: &UserId, room_id: &RoomId) -> Result {
	services
		.membership
		.join(Join {
			sender_user: user_id,
			room_id,
			orig_room_id: None,
			reason: None,
			servers: &[],
			is_appservice: false,
			extra_content: None,
		})
		.await
}

async fn ban(services: &Services, admin: &UserId, user_id: &UserId, room_id: &RoomId) -> Result {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.membership
		.ban(room_id, user_id, None, admin, &state_lock)
		.await
}

async fn set_knock_rule(services: &Services, admin: &UserId, room_id: &RoomId) -> Result {
	let content = RoomJoinRulesEventContent::new(JoinRule::Knock);
	let builder = PduBuilder::state(String::new(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, admin, room_id, &state_lock)
		.await
		.map(drop)
}

async fn assert_malformed(
	services: &Services,
	base: &str,
	route: Route,
	room_id: &RoomId,
	event: &MembershipPdu,
) -> Result {
	let mut pdu: Value = serde_json::from_str(event.pdu.get())?;

	pdu["type"] = json!("m.room.topic");

	let response = route
		.send(services, base, room_id, &event.event_id, to_raw_value(&pdu)?)
		.await?;

	assert_eq!(response.0, 400, "{route:?}: {}", response.1);

	Ok(())
}

async fn assert_refused(
	services: &Services,
	base: &str,
	route: Route,
	room_id: &RoomId,
	event: &MembershipPdu,
) -> Result {
	let response = route
		.send(services, base, room_id, &event.event_id, event.pdu.clone())
		.await?;

	assert_eq!(response.0, 403, "{route:?}: {}", response.1);
	assert_eq!(response.1["errcode"], "M_FORBIDDEN", "{route:?}");

	assert!(
		services
			.pdu_metadata
			.is_event_soft_failed(&event.event_id)
			.await,
		"{route:?}: missing soft-fail marker",
	);

	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(&event.event_id)
			.await
			.is_err_and(|error| error.is_not_found()),
		"{route:?}: refused event entered the timeline",
	);

	Ok(())
}

impl Route {
	async fn send(
		self,
		services: &Services,
		base: &str,
		room_id: &RoomId,
		event_id: &OwnedEventId,
		pdu: Box<RawJsonValue>,
	) -> Result<(u16, Value)> {
		match self {
			| Self::Join => {
				let request = JoinRequest::new(room_id.to_owned(), event_id.clone(), pdu);

				send(services, base, request).await
			},
			| Self::Leave => {
				let request = LeaveRequest::new(room_id.to_owned(), event_id.clone(), pdu);

				send(services, base, request).await
			},
			| Self::Knock => {
				let request = KnockRequest::new(room_id.to_owned(), event_id.clone(), pdu);

				send(services, base, request).await
			},
		}
	}
}

async fn send<T>(services: &Services, base: &str, request: T) -> Result<(u16, Value)>
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
	let body = response.json().await?;

	Ok((status, body))
}
