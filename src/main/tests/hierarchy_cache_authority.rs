#![cfg(test)]
#![recursion_limit = "256"]

#[path = "client/mod.rs"]
#[expect(
	dead_code,
	reason = "the shared fixture needs only readiness polling"
)]
mod client;
#[path = "fixture/mod.rs"]
mod fixture;

use std::{
	convert::identity,
	net::TcpListener,
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering::SeqCst},
	},
	time::Duration,
};

use axum::{
	Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::any,
};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use futures::future::join;
use serde_json::{Value, json};
use tokio::{spawn, sync::Notify, time::timeout};
use tuwunel_core::{
	Err, PduCount, Result, err,
	ruma::{
		OwnedRoomId, OwnedServerName, RoomId, ServerName, UInt, UserId,
		events::room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tuwunel_database::Json as DbJson;
use tuwunel_service::{
	Services,
	rooms::{
		spaces::{Accessibility, Identifier},
		state_cache::MembershipUpdate,
	},
	users::Register,
};

use self::fixture::boot;

const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";
const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";
const TIMEOUT: Duration = Duration::from_secs(20);

struct Peer {
	arrived: Arc<Notify>,
	release: Option<Arc<Notify>>,
	status: StatusCode,
	response: Value,
	hits: AtomicUsize,
}

struct Exercise<'a> {
	services: &'a Services,
	base: &'a str,
	bad_listener: TcpListener,
	good_listener: TcpListener,
	failed_listener: TcpListener,
	bad: OwnedServerName,
	good: OwnedServerName,
	failed: OwnedServerName,
}

struct HierarchyChildren<'a> {
	positive: &'a str,
	inaccessible: &'a str,
	space: &'a str,
	joined_positive: &'a str,
	joined_negative: &'a str,
}

#[test]
fn hierarchy_rejects_wrong_root_and_prefers_joined_local_state() -> Result {
	let bad_listener = TcpListener::bind(("127.0.0.1", 0))?;
	let good_listener = TcpListener::bind(("127.0.0.1", 0))?;
	let failed_listener = TcpListener::bind(("127.0.0.1", 0))?;
	let bad = server_name(&bad_listener)?;
	let good = server_name(&good_listener)?;
	let failed = server_name(&failed_listener)?;

	bad_listener.set_nonblocking(true)?;
	good_listener.set_nonblocking(true)?;
	failed_listener.set_nonblocking(true)?;

	let options = [
		"allow_invalid_tls_certificates=true",
		"ip_range_denylist=[]",
		"federation_loopback=true",
		"log=\"error\"",
	];

	boot("hierarchy-cache-authority", options, async move |services, base| {
		Box::pin(exercise(Exercise {
			services,
			base,
			bad_listener,
			good_listener,
			failed_listener,
			bad,
			good,
			failed,
		}))
		.await
	})
}

#[tracing::instrument(level = "debug", skip_all)]
#[expect(
	clippy::too_many_lines,
	reason = "one fixture preserves the ordered race schedule"
)]
async fn exercise(args: Exercise<'_>) -> Result {
	let Exercise {
		services,
		base,
		bad_listener,
		good_listener,
		failed_listener,
		bad,
		good,
		failed,
	} = args;
	let user = UserId::parse_with_server_name("hierarchy", services.globals.server_name())?;
	let token = "hierarchy-cache-authority-token-0000000001";

	services
		.users
		.full_register(Register {
			user_id: Some(&user),
			password: Some("hierarchy-cache-authority-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user, None, (Some(token), None), None, None, None)
		.await?;

	let room_id = create_room(services, base, token).await?;
	let identifier = Identifier::UserId(&user);
	let Accessibility::Accessible(local) = services
		.spaces
		.get_summary_and_children(&room_id, &identifier, &[])
		.await?
	else {
		return Err!("new local room was inaccessible");
	};

	transition(services, &room_id, &user, MembershipState::Leave).await?;

	let bad_arrived = Arc::new(Notify::new());
	let good_arrived = Arc::new(Notify::new());
	let failed_arrived = Arc::new(Notify::new());
	let release = Arc::new(Notify::new());
	let failed_release = Arc::new(Notify::new());
	let positive_child = RoomId::parse("!positive-child:example.org")?;
	let inaccessible_child = RoomId::parse("!inaccessible-child:example.org")?;
	let good_positive = RoomId::parse("!good-positive:example.org")?;
	let good_negative = RoomId::parse("!good-negative:example.org")?;
	let good_space = RoomId::parse("!good-space:example.org")?;
	let joined_positive = create_room(services, base, token).await?;
	let joined_negative = create_room(services, base, token).await?;
	let bad_response = hierarchy_response_with_sentinels(
		"!wrong:example.org",
		"wrong",
		positive_child.as_str(),
		inaccessible_child.as_str(),
	);
	let bad_state = Arc::new(Peer {
		arrived: bad_arrived.clone(),
		release: None,
		status: StatusCode::OK,
		response: bad_response,
		hits: AtomicUsize::new(0),
	});

	let good_response =
		hierarchy_response_with_children(room_id.as_str(), "remote", &HierarchyChildren {
			positive: good_positive.as_str(),
			inaccessible: good_negative.as_str(),
			space: good_space.as_str(),
			joined_positive: joined_positive.as_str(),
			joined_negative: joined_negative.as_str(),
		});
	let good_state = Arc::new(Peer {
		arrived: good_arrived.clone(),
		release: Some(release.clone()),
		status: StatusCode::OK,
		response: good_response,
		hits: AtomicUsize::new(0),
	});

	let failed_state = Arc::new(Peer {
		arrived: failed_arrived.clone(),
		release: Some(failed_release.clone()),
		status: StatusCode::INTERNAL_SERVER_ERROR,
		response: json!({}),
		hits: AtomicUsize::new(0),
	});

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let certificate = manifest.join(CERTIFICATE);
	let private_key = manifest.join(PRIVATE_KEY);
	let bad_stub = spawn(serve_peer(
		bad_listener,
		bad_state.clone(),
		certificate.clone(),
		private_key.clone(),
	));

	let good_stub =
		spawn(serve_peer(good_listener, good_state.clone(), certificate, private_key));

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let failed_stub = spawn(serve_peer(
		failed_listener,
		failed_state.clone(),
		manifest.join(CERTIFICATE),
		manifest.join(PRIVATE_KEY),
	));

	let bad_via = [bad.clone()];
	let via = [bad, good];
	let failed_via = [failed];

	let rejected = services
		.spaces
		.get_summary_and_children(&room_id, &identifier, &bad_via)
		.await;

	if !rejected.is_err_and(|error| error.is_not_found()) {
		return Err!("wrong-root-only hierarchy was not rejected as not found");
	}

	assert_cache_absent(services, &inaccessible_child).await?;

	for child in [&positive_child, &inaccessible_child] {
		let rejected = services
			.spaces
			.get_summary_and_children(child, &identifier, &bad_via)
			.await;

		if !rejected.is_err_and(|error| error.is_not_found()) {
			return Err!("wrong-root child sentinel entered the hierarchy cache");
		}
	}

	if bad_state.hits.load(SeqCst) != 3 {
		return Err!("child sentinel checks did not reach the federation peer");
	}

	let request = services
		.spaces
		.get_summary_and_children(&room_id, &identifier, &via);

	let membership = async {
		join(bad_arrived.notified(), good_arrived.notified()).await;
		transition(services, &room_id, &user, MembershipState::Join).await?;
		release.notify_one();
		Ok::<_, tuwunel_core::Error>(())
	};

	let outcome = timeout(
		TIMEOUT,
		Box::pin(async {
			let (summary, membership) = join(request, membership).await;

			membership?;

			let Accessibility::Accessible(summary) = summary? else {
				return Err!("joined room was inaccessible");
			};

			if summary.summary.room_id != room_id
				|| summary.summary.name.as_deref() != Some("local")
			{
				return Err!("federation response masked joined local room state");
			}

			if bad_state.hits.load(SeqCst) != 4 || good_state.hits.load(SeqCst) != 1 {
				return Err!("hierarchy request did not exercise both via servers");
			}

			for child in [&good_positive, &good_negative, &good_space] {
				assert_cache_absent(services, child).await?;
			}

			transition(services, &room_id, &user, MembershipState::Leave).await?;
			let failed_request =
				services
					.spaces
					.get_summary_and_children(&room_id, &identifier, &failed_via);

			let membership = async {
				failed_arrived.notified().await;
				transition(services, &room_id, &user, MembershipState::Join).await?;
				failed_release.notify_one();
				Ok::<_, tuwunel_core::Error>(())
			};

			let (summary, membership) = join(failed_request, membership).await;

			membership?;
			let Accessibility::Accessible(summary) = summary? else {
				return Err!("joined room was inaccessible after federation failure");
			};

			if serde_json::to_value(&summary)? != serde_json::to_value(&local)? {
				return Err!("federation failure did not return exact local metadata");
			}

			if failed_state.hits.load(SeqCst) != 1 {
				return Err!("failed federation completion was not exercised");
			}

			for child in [&joined_positive, &joined_negative] {
				if !matches!(
					services
						.spaces
						.get_summary_and_children(child, &identifier, &[])
						.await?,
					Accessibility::Accessible(_)
				) {
					return Err!("joined child local summary was inaccessible");
				}
			}

			let joined_positive_cache = cache_json(services, &joined_positive)
				.await?
				.ok_or_else(|| err!("joined positive cache was absent"))?;
			let mut joined_negative_cache = joined_positive_cache.clone();

			joined_negative_cache["summary"] = Value::Null;
			services.db["roomid_spacehierarchy"]
				.raw_put(&joined_positive, DbJson(joined_positive_cache.clone()));
			services.db["roomid_spacehierarchy"]
				.raw_put(&joined_negative, DbJson(joined_negative_cache.clone()));

			transition(services, &room_id, &user, MembershipState::Leave).await?;
			let Accessibility::Accessible(summary) = services
				.spaces
				.get_summary_and_children(&room_id, &identifier, &via)
				.await?
			else {
				return Err!("remote room was inaccessible after leaving");
			};

			if summary.summary.room_id != room_id
				|| summary.summary.name.as_deref() != Some("remote")
			{
				return Err!("local cache remained authoritative after leaving");
			}

			if bad_state.hits.load(SeqCst) != 5 || good_state.hits.load(SeqCst) != 2 {
				return Err!("leave did not fetch a remote hierarchy");
			}

			assert_remote_cache(services, &good_positive, true).await?;
			assert_remote_cache(services, &good_negative, false).await?;
			assert_cache_absent(services, &good_space).await?;
			assert_eq!(
				cache_json(services, &joined_positive).await?,
				Some(joined_positive_cache)
			);
			assert_eq!(
				cache_json(services, &joined_negative).await?,
				Some(joined_negative_cache)
			);

			for child in [&joined_positive, &joined_negative] {
				let Accessibility::Accessible(summary) = services
					.spaces
					.get_summary_and_children(child, &identifier, &[])
					.await?
				else {
					return Err!("preserved joined child was inaccessible");
				};

				if summary.summary.room_id != **child {
					return Err!("joined child returned nonlocal metadata");
				}
			}

			let mut legacy = cache_json(services, &room_id)
				.await?
				.ok_or_else(|| err!("remote hierarchy cache was absent"))?;

			legacy
				.as_object_mut()
				.ok_or_else(|| err!("remote hierarchy cache was not an object"))?
				.remove("provenance");

			services.db["roomid_spacehierarchy"].raw_put(&room_id, DbJson(legacy));

			let Accessibility::Accessible(summary) = services
				.spaces
				.get_summary_and_children(&room_id, &identifier, &via)
				.await?
			else {
				return Err!("legacy hierarchy cache did not refetch remotely");
			};

			if summary.summary.name.as_deref() != Some("remote") {
				return Err!("legacy hierarchy refetch returned unexpected metadata");
			}

			if bad_state.hits.load(SeqCst) != 6 || good_state.hits.load(SeqCst) != 3 {
				return Err!("legacy hierarchy cache suppressed federation refetch");
			}

			transition(services, &room_id, &user, MembershipState::Join).await?;
			let Accessibility::Accessible(summary) = services
				.spaces
				.get_summary_and_children(&room_id, &identifier, &via)
				.await?
			else {
				return Err!("rejoined room was inaccessible");
			};

			if summary.summary.room_id != room_id
				|| summary.summary.name.as_deref() != Some("local")
			{
				return Err!("remote cache remained authoritative after joining");
			}

			if bad_state.hits.load(SeqCst) != 6 || good_state.hits.load(SeqCst) != 3 {
				return Err!("join unexpectedly fetched a remote hierarchy");
			}

			let good_sender = Identifier::ServerName(via[1].as_ref());

			if !matches!(
				services
					.spaces
					.get_summary_and_children(&room_id, &good_sender, &[])
					.await?,
				Accessibility::Inaccessible
			) {
				return Err!("federation sender bypassed the local join rule");
			}

			if bad_state.hits.load(SeqCst) != 6 || good_state.hits.load(SeqCst) != 3 {
				return Err!("federation sender unexpectedly fetched remote hierarchy");
			}

			Ok(())
		}),
	)
	.await
	.map_err(|_| err!("hierarchy authority exercise timed out"))
	.and_then(identity);

	bad_stub.abort();
	good_stub.abort();
	failed_stub.abort();
	outcome
}

async fn create_room(services: &Services, base: &str, token: &str) -> Result<OwnedRoomId> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/createRoom"))
		.bearer_auth(token)
		.json(&json!({"name": "local"}))
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

async fn transition(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	membership: MembershipState,
) -> Result {
	services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id,
			user_id,
			membership_event: RoomMemberEventContent::new(membership),
			sender: user_id,
			last_state: None,
			invite_via: None,
			update_joined_count: true,
			count: PduCount::Normal(*services.globals.next_count()),
		})
		.await
}

async fn serve_peer(
	listener: TcpListener,
	state: Arc<Peer>,
	certificate: PathBuf,
	private_key: PathBuf,
) -> Result {
	let config = RustlsConfig::from_pem_file(certificate, private_key).await?;
	let app = Router::new()
		.route("/_matrix/federation/{*rest}", any(answer_peer))
		.with_state(state);

	from_tcp_rustls(listener, config)?
		.serve(app.into_make_service())
		.await?;

	Ok(())
}

async fn answer_peer(State(state): State<Arc<Peer>>) -> impl IntoResponse {
	let hit = state.hits.fetch_add(1, SeqCst);

	state.arrived.notify_one();

	if hit == 0
		&& let Some(release) = &state.release
	{
		release.notified().await;
	}

	(state.status, Json(state.response.clone()))
}

fn hierarchy_response(room_id: &str, name: &str) -> Value {
	json!({
		"room": {
			"room_id": room_id,
			"join_rule": "public",
			"world_readable": true,
			"guest_can_join": true,
			"num_joined_members": UInt::from(1_u32),
			"children_state": [],
			"name": name,
		},
		"children": [],
		"inaccessible_children": [],
	})
}

fn hierarchy_response_with_sentinels(
	room_id: &str,
	name: &str,
	positive_child: &str,
	inaccessible_child: &str,
) -> Value {
	let mut response = hierarchy_response(room_id, name);

	response["children"] = json!([{
		"room_id": positive_child,
		"join_rule": "public",
		"world_readable": true,
		"guest_can_join": true,
		"num_joined_members": UInt::from(1_u32),
		"children_state": [],
		"name": "positive sentinel",
	}]);

	response["inaccessible_children"] = json!([inaccessible_child]);
	response
}

fn hierarchy_response_with_children(
	room_id: &str,
	name: &str,
	children: &HierarchyChildren<'_>,
) -> Value {
	let &HierarchyChildren {
		positive,
		inaccessible,
		space,
		joined_positive,
		joined_negative,
	} = children;
	let mut response = hierarchy_response(room_id, name);

	response["children"] = json!([
		{
			"room_id": positive,
			"join_rule": "public",
			"world_readable": true,
			"guest_can_join": true,
			"num_joined_members": UInt::from(1_u32),
			"children_state": [],
			"name": "positive child",
		},
		{
			"room_id": space,
			"join_rule": "public",
			"world_readable": true,
			"guest_can_join": true,
			"num_joined_members": UInt::from(1_u32),
			"children_state": [],
			"room_type": "m.space",
			"name": "space child",
		},
		{
			"room_id": joined_positive,
			"join_rule": "public",
			"world_readable": true,
			"guest_can_join": true,
			"num_joined_members": UInt::from(1_u32),
			"children_state": [],
			"name": "joined positive child",
		},
	]);

	response["inaccessible_children"] = json!([inaccessible, joined_negative]);
	response
}

async fn cache_json(services: &Services, room_id: &RoomId) -> Result<Option<Value>> {
	match services.db["roomid_spacehierarchy"]
		.get(room_id)
		.await
	{
		| Ok(value) => Ok(Some(serde_json::from_slice(&value)?)),
		| Err(error) if error.is_not_found() => Ok(None),
		| Err(error) => Err(error),
	}
}

async fn assert_cache_absent(services: &Services, room_id: &RoomId) -> Result {
	if cache_json(services, room_id).await?.is_some() {
		return Err!("unexpected hierarchy cache entry for {room_id}");
	}

	Ok(())
}

async fn assert_remote_cache(services: &Services, room_id: &RoomId, positive: bool) -> Result {
	let cached = cache_json(services, room_id)
		.await?
		.ok_or_else(|| err!("missing hierarchy cache entry for {room_id}"))?;

	if cached.get("provenance").and_then(Value::as_str) != Some("remote") {
		return Err!("hierarchy cache entry lacked remote provenance for {room_id}");
	}

	if cached.get("summary").is_some_and(Value::is_null) == positive {
		return Err!("hierarchy cache entry had wrong polarity for {room_id}");
	}

	Ok(())
}

fn server_name(listener: &TcpListener) -> Result<OwnedServerName> {
	Ok(ServerName::parse(format!("127.0.0.1:{}", listener.local_addr()?.port()))?)
}
