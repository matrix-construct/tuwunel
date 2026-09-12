#![cfg(test)]

use std::{
	env::{current_exe, temp_dir, var},
	fs::remove_dir_all,
	path::{Path, PathBuf},
	process::{Command, id as process_id},
	sync::Arc,
};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	config::ServerUserLocalpart,
	pdu::PduBuilder,
	ruma::{
		OwnedRoomId, OwnedUserId, RoomId, UserId,
		events::{
			StateEventContent,
			room::{
				create::RoomCreateEventContent,
				join_rules::{JoinRule, RoomJoinRulesEventContent},
				member::{MembershipState, RoomMemberEventContent},
			},
		},
	},
	utils::result::NotFound,
};
use tuwunel_database::Deserialized;
use tuwunel_service::{Services, rooms::state::RoomMutexGuard, users::SERVER_USER_KEY};

const CHILD_DATABASE_ENV: &str = "SERVER_USER_IDENTITY_TEST_DATABASE";
const CHILD_PHASE_ENV: &str = "SERVER_USER_IDENTITY_TEST_PHASE";

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// The identity is stamped on the first boot and compared exactly afterwards.
///
/// A database from before the stamp keeps the legacy identity even when the
/// server user no longer matches its admin room, as after a hand-rebuilt room.
/// Any other identity is refused unless an account of that name is joined
/// there. Separate child processes boot the one database six times.
#[test]
fn server_user_identity_is_durable() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		let database: PathBuf = var(CHILD_DATABASE_ENV)
			.expect("server user identity child database is configured")
			.into();

		return match phase.as_str() {
			| "seed" => seed_phase(&database),
			| "foreign" => foreign_phase(&database),
			| "read_only" => read_only_phase(&database),
			| "legacy" => legacy_phase(&database),
			| "adopted" => adopted_phase(&database),
			| "changed" => changed_phase(&database),
			| _ => Err!("unknown server user identity child phase: {phase}"),
		};
	}

	let path = temp_dir()
		.join("tuwunel")
		.join(format!("server-user-identity-{}", process_id()));

	let database = DatabasePath(path);

	for phase in ["seed", "foreign", "read_only", "legacy", "adopted", "changed"] {
		run_child(&database.0, phase)?;
	}

	Ok(())
}

/// Boots fresh, then leaves behind a pre-stamp database whose admin alias
/// names a room a human administrator created.
///
/// The default identity boots unstamped while the server user is outside that
/// room and is stamped once joined; the stamp is then removed again, which is
/// the state of every database from before the stamp existed.
fn seed_phase(database: &Path) -> Result {
	let args = Args::default_test(&["fresh"])
		.with_option(format!("database_path={database:?}"))
		.with_option("create_admin_room=true");

	boot(&args, async |services| {
		assert_eq!(established(services).await?.as_deref(), Some("conduit"));

		let admin_alias = &services.admin.admin_alias;
		let (human, room_id) = rebuild_admin_room(services).await?;

		services.alias.remove_alias(admin_alias).await?;
		services.alias.set_alias(admin_alias, &room_id)?;
		services.db["global"].remove(SERVER_USER_KEY);
		services.users.validate_server_user().await?;
		assert_eq!(established(services).await?, None);

		join_user(services, &human, &services.globals.server_user, &room_id).await?;
		services.users.validate_server_user().await?;
		assert_eq!(established(services).await?.as_deref(), Some("conduit"));
		services.db["global"].remove(SERVER_USER_KEY);

		Ok(())
	})
}

/// Creates a room the way an operator rebuilds a deleted admin room: by hand,
/// with themselves as the creator.
///
/// The create, join and join-rules events are the minimum that lets the
/// server user be invited later.
async fn rebuild_admin_room(services: &Services) -> Result<(OwnedUserId, OwnedRoomId)> {
	let human = user(services, "operator")?;
	let room_id = RoomId::new_v1(services.globals.server_name());

	services
		.users
		.create(&human, Some("operator-password"), None)
		.await?;

	services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let state_lock = services.state.mutex.lock(&room_id).await;
	let create = RoomCreateEventContent::new_v11();
	let join = RoomMemberEventContent::new(MembershipState::Join);
	let join_rules = RoomJoinRulesEventContent::new(JoinRule::Invite);

	append_state(services, &room_id, &state_lock, &human, "", &create).await?;
	append_state(services, &room_id, &state_lock, &human, human.as_str(), &join).await?;
	append_state(services, &room_id, &state_lock, &human, "", &join_rules).await?;

	Ok((human, room_id))
}

/// Builds a user ID on this server from a localpart.
fn user(services: &Services, localpart: &str) -> Result<OwnedUserId> {
	UserId::parse_with_server_name(localpart, services.globals.server_name()).map_err(Into::into)
}

/// Appends one state event to the room the lock guards.
async fn append_state<T: StateEventContent + Sync>(
	services: &Services,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
	sender: &UserId,
	state_key: &str,
	content: &T,
) -> Result {
	services
		.timeline
		.build_and_append_pdu(PduBuilder::state(state_key, content), sender, room_id, state_lock)
		.await
		.map(|_| ())
}

/// Invites the joiner into the rebuilt room on the human's behalf and joins
/// it by direct append.
///
/// Membership is what stamps the legacy identity and what admits the adopted
/// one.
async fn join_user(
	services: &Services,
	human: &UserId,
	joiner: &UserId,
	room_id: &RoomId,
) -> Result {
	let state_lock = services.state.mutex.lock(room_id).await;
	let invite = RoomMemberEventContent::new(MembershipState::Invite);
	let join = RoomMemberEventContent::new(MembershipState::Join);

	append_state(services, room_id, &state_lock, human, joiner.as_str(), &invite).await?;
	append_state(services, room_id, &state_lock, joiner, joiner.as_str(), &join).await
}

/// A pre-stamp database refuses an identity its admin room does not include.
///
/// The seeded room was created by a human administrator, which never decides
/// the identity on its own; only the missing membership does.
fn foreign_phase(database: &Path) -> Result {
	refused(database, "_server", "establishes that identity")
}

/// A read-only boot of the pre-stamp database starts without stamping it.
///
/// Migrations are skipped because the identity check runs regardless and one
/// of them still writes under a read-only engine.
fn read_only_phase(database: &Path) -> Result {
	let args = Args::default_test(&[])
		.with_option(format!("database_path={database:?}"))
		.with_option("rocksdb_read_only=true")
		.with_option("database_migrations=false");

	boot(&args, async |services| {
		assert_eq!(established(services).await?, None);

		Ok(())
	})
}

/// Boots the legacy identity on the pre-stamp database and stamps it.
///
/// The rebuilt admin room must not disqualify the default. The phase then
/// admits a foreign account to the room and strips the stamp again, leaving
/// the database for the adoption that follows.
fn legacy_phase(database: &Path) -> Result {
	let args = Args::default_test(&[]).with_option(format!("database_path={database:?}"));

	boot(&args, async |services| {
		assert_eq!(established(services).await?.as_deref(), Some("conduit"));

		let room_id = services
			.alias
			.resolve_local_alias(&services.admin.admin_alias)
			.await?;

		let human = user(services, "operator")?;
		let foreign = user(services, "_server")?;

		services
			.users
			.create(&foreign, None, None)
			.await?;

		join_user(services, &human, &foreign, &room_id).await?;
		services.db["global"].remove(SERVER_USER_KEY);

		Ok(())
	})
}

/// A pre-stamp database adopts a nondefault identity its admin room includes.
///
/// The room's human creator counts for nothing; the membership alone stamps
/// it.
fn adopted_phase(database: &Path) -> Result {
	let args = Args::default_test(&[])
		.with_option(format!("database_path={database:?}"))
		.with_option("server_user_localpart=\"_server\"");

	boot(&args, async |services| {
		assert_eq!(established(services).await?.as_deref(), Some("_server"));

		Ok(())
	})
}

/// A changed identity is refused by the stamp before the server starts.
///
/// The stamp the adoption wrote decides here; the admin room is not read.
fn changed_phase(database: &Path) -> Result {
	refused(database, "conduit", "established _server")
}

/// Starts the server under the given identity expecting the guard to refuse it.
///
/// Startup itself fails, so the shutdown ladder of `boot` never runs here.
fn refused(database: &Path, localpart: &str, refusal: &str) -> Result {
	let args = Args::default_test(&[])
		.with_option(format!("database_path={database:?}"))
		.with_option(format!("server_user_localpart={localpart:?}"));

	let (runtime, server) = start(&args)?;
	let result = runtime.block_on(async_start(&server));

	drop(runtime);

	let error = result
		.map(drop)
		.expect_err("a refused identity must not start");

	assert!(error.to_string().contains(refusal), "{error}");

	Ok(())
}

fn run_child(database: &Path, phase: &str) -> Result {
	let output = Command::new(current_exe()?)
		.env(CHILD_DATABASE_ENV, database)
		.env(CHILD_PHASE_ENV, phase)
		.output()?;

	if !output.status.success() {
		let stdout = String::from_utf8_lossy(&output.stdout);
		let stderr = String::from_utf8_lossy(&output.stderr);

		return Err!(
			"server user identity {phase} child failed with \
			 {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
			output.status,
		);
	}

	Ok(())
}

fn boot<F>(args: &Args, exercise: F) -> Result
where
	F: AsyncFnOnce(&Services) -> Result,
{
	let (runtime, server) = start(args)?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

fn start(args: &Args) -> Result<(Runtime, Arc<Server>)> {
	let runtime = Runtime::new(Some(args))?;
	let server = Server::new(Some(args), Some(&runtime))?;

	Ok((runtime, server))
}

async fn established(services: &Services) -> Result<Option<ServerUserLocalpart>> {
	services.db["global"]
		.get(SERVER_USER_KEY)
		.await
		.deserialized()
		.optional()
}
