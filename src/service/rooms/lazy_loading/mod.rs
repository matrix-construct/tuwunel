//! Lazy Loading

use std::{collections::HashSet, sync::Arc};

use futures::{Stream, StreamExt, pin_mut};
use ruma::{DeviceId, OwnedUserId, RoomId, UserId, api::client::filter::LazyLoadOptions};
use tuwunel_core::{
	Result, implement,
	utils::{IterStream, ReadyExt, stream::TryIgnore},
};
use tuwunel_database::{Database, Deserialized, Handle, Interfix, Map, Qry};

pub struct Service {
	db: Data,
}

struct Data {
	lazyloadedids: Arc<Map>,
	db: Arc<Database>,
}

pub trait Options: Send + Sync {
	fn is_enabled(&self) -> bool;
	fn include_redundant_members(&self) -> bool;
}

#[derive(Clone, Debug)]
pub struct Context<'a> {
	pub user_id: &'a UserId,
	pub device_id: Option<&'a DeviceId>,
	pub room_id: &'a RoomId,
	pub token: Option<u64>,
	pub options: Option<&'a LazyLoadOptions>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
	Unseen,
	Seen(u64),
}

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
		if include_redundant || status == Status::Unseen {
			senders.insert(sender.into());
			continue;
		}

		if let Status::Seen(seen) = status
			&& (seen == 0 || ctx.token == Some(seen))
		{
			senders.insert(sender.into());
			continue;
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
	let make_key =
		|sender: &'a UserId| -> Key<'a> { (ctx.user_id, ctx.device_id, ctx.room_id, sender) };

	senders
		.clone()
		.stream()
		.map(make_key)
		.qry(&self.db.lazyloadedids)
		.map(into_status)
		.zip(senders.stream())
		.map(move |(status, sender)| {
			if matches!(status, Status::Unseen) {
				self.db
					.lazyloadedids
					.put_aput::<8, _, _>(make_key(sender), 0_u64);
			} else if matches!(status, Status::Seen(0)) {
				self.db
					.lazyloadedids
					.put_aput::<8, _, _>(make_key(sender), ctx.token.unwrap_or(0_u64));
			}

			status
		})
}

fn into_status(result: Result<Handle<'_>>) -> Status {
	match result.and_then(|handle| handle.deserialized()) {
		| Ok(seen) => Status::Seen(seen),
		| Err(_) => Status::Unseen,
	}
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
