//! Lazy-loaded room membership tracking.
//!
//! The service records which member events lazy-loading-aware endpoints have sent to each user and
//! device in a room. Callers use that history to omit redundant membership events or update the
//! witness state.

use std::{collections::HashSet, sync::Arc};

use futures::{Stream, StreamExt, pin_mut};
use ruma::{DeviceId, OwnedUserId, RoomId, UserId, api::client::filter::LazyLoadOptions};
use tuwunel_core::{
	Result, implement,
	utils::{IterStream, ReadyExt, stream::TryIgnore},
};
use tuwunel_database::{Database, Deserialized, Handle, Interfix, Map, Qry};

/// Tracks room members previously sent through lazy-loading-aware endpoints.
///
/// Witness rows are scoped by receiving user, optional device, room, and member. The stored value
/// records the caller-provided position associated with the latest visibility transition.
pub struct Service {
	db: Data,
}

struct Data {
	lazyloadedids: Arc<Map>,
	db: Arc<Database>,
}

/// Exposes the lazy-loading decisions needed by the service.
///
/// Implementations adapt request filter types without coupling the storage logic to their concrete
/// representation.
pub trait Options: Send + Sync {
	/// Reports whether lazy loading is enabled.
	///
	/// Disabled options must not be passed to witness filtering.
	fn is_enabled(&self) -> bool;

	/// Reports whether previously seen members should be included again.
	///
	/// When enabled, witness history does not remove candidates from the response.
	fn include_redundant_members(&self) -> bool;
}

/// Parameters that scope and control one lazy-loading operation.
///
/// The identity fields select a witness namespace. The token, options, and mode determine which
/// member events are returned and whether their state is advanced.
#[derive(Clone, Debug)]
pub struct Context<'a> {
	/// User receiving the room membership events.
	pub user_id: &'a UserId,

	/// Device receiving the events, or the user-wide scope when absent.
	pub device_id: Option<&'a DeviceId>,

	/// Room whose membership events are being filtered.
	pub room_id: &'a RoomId,

	/// Caller-provided position token used when advancing an intermediate witness.
	pub token: Option<u64>,

	/// Client lazy-loading options for the operation.
	pub options: Option<&'a LazyLoadOptions>,

	/// Read, update, or prefetch behavior for the witness lookup.
	pub mode: Mode,
}

/// Selects how a lazy-loading lookup interacts with witness state.
///
/// Read mode filters from stored state, update mode also advances it, and prefetch mode performs
/// lookups without returning candidates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
	/// Reads witness state without modifying it.
	Read,

	/// Reads witness state and advances new or intermediate entries.
	Update,

	/// Performs witness lookups without returning or updating members.
	Prefetch,
}

/// Describes whether a member has been witnessed in the selected scope.
///
/// A seen value records the stored caller-provided position. Zero marks the intermediate state
/// before a later update assigns the current token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
	/// No witness row exists for the member.
	Unseen,

	/// The member was witnessed at the contained caller-provided position.
	Seen(u64),
}

/// Set of room members considered for lazy-loaded inclusion.
///
/// Filtering consumes a witness set and returns the members that should be sent to the client.
pub type Witness = HashSet<OwnedUserId>;
type Key<'a> = (&'a UserId, Option<&'a DeviceId>, &'a RoomId, &'a UserId);

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				lazyloadedids: args.db["lazyloadedids"].clone(),
				db: args.db.clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Clears witness history for one user, device, and room scope.
///
/// Every member row under the context prefix is removed. Unreadable rows encountered during the
/// scan are skipped.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn reset(&self, ctx: &Context<'_>) {
	let prefix = (ctx.user_id, ctx.device_id, ctx.room_id, Interfix);
	self.db
		.lazyloadedids
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_for_each(|key| self.db.lazyloadedids.remove(key))
		.await;
}

/// Retains the member events required by lazy-loading state.
///
/// Candidates in `Unseen`, `Seen(0)`, or a `Seen` state matching the context token are retained
/// unless either client options or the build configuration requests redundant members, in which
/// case every candidate is retained. Update mode advances witness rows, while prefetch mode performs
/// the lookups and returns an empty set.
#[implement(Service)]
#[tracing::instrument(name = "retain", level = "debug", skip_all)]
pub async fn witness_retain(&self, senders: Witness, ctx: &Context<'_>) -> Witness {
	debug_assert!(
		ctx.options.is_none_or(Options::is_enabled),
		"lazy loading should be enabled by your options"
	);

	let include_redundant = cfg!(feature = "element_hacks")
		|| ctx
			.options
			.is_some_and(Options::include_redundant_members);

	let witness = self
		.witness(ctx, senders.iter().map(AsRef::as_ref))
		.zip(senders.iter().stream());

	pin_mut!(witness);
	let _cork = self.db.db.cork();
	let mut senders = Witness::with_capacity(senders.len());
	while let Some((status, sender)) = witness.next().await {
		if ctx.mode == Mode::Prefetch {
			continue;
		}

		if include_redundant || status == Status::Unseen {
			senders.insert(sender.into());
			continue;
		}

		if let Status::Seen(seen) = status
			&& (seen == 0 || ctx.token == Some(seen))
		{
			senders.insert(sender.into());
		}
	}

	senders
}

#[implement(Service)]
fn witness<'a, I>(
	&'a self,
	ctx: &'a Context<'a>,
	senders: I,
) -> impl Stream<Item = Status> + Send + 'a
where
	I: Iterator<Item = &'a UserId> + Send + Clone + 'a,
{
	senders
		.clone()
		.stream()
		.map(|sender| make_key(ctx, sender))
		.qry(&self.db.lazyloadedids)
		.map(into_status)
		.zip(senders.stream())
		.map(move |(status, sender)| {
			if matches!(ctx.mode, Mode::Update) {
				self.update(ctx, &status, sender);
			}

			status
		})
}

#[implement(Service)]
fn update(&self, ctx: &Context<'_>, status: &Status, sender: &UserId) {
	if matches!(status, Status::Unseen) {
		self.db
			.lazyloadedids
			.put_aput::<8, _, _>(make_key(ctx, sender), 0_u64);
	} else if matches!(status, Status::Seen(0)) {
		self.db
			.lazyloadedids
			.put_aput::<8, _, _>(make_key(ctx, sender), ctx.token.unwrap_or(0_u64));
	}
}

fn into_status(result: Result<Handle<'_>>) -> Status {
	match result.and_then(|handle| handle.deserialized()) {
		| Ok(seen) => Status::Seen(seen),
		| Err(_) => Status::Unseen,
	}
}

fn make_key<'a>(ctx: &'a Context<'a>, sender: &'a UserId) -> Key<'a> {
	(ctx.user_id, ctx.device_id, ctx.room_id, sender)
}

impl Options for LazyLoadOptions {
	fn include_redundant_members(&self) -> bool {
		if let Self::Enabled { include_redundant_members } = self {
			*include_redundant_members
		} else {
			false
		}
	}

	fn is_enabled(&self) -> bool { !self.is_disabled() }
}
