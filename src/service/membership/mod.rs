mod auto_accept;
mod ban;
mod invite;
mod join;
mod kick;
mod knock;
mod leave;
mod stripped_state;
mod unban;

use std::sync::Arc;

use async_trait::async_trait;
use loole::{Receiver, Sender, unbounded};
use tuwunel_core::Result;

use self::auto_accept::Pending;
pub use self::{
	join::Join,
	stripped_state::{
		StrippedCreateVerdict, dedup_stripped_state, enforce_stripped_create,
		into_client_stripped, v12_room_ids, without_member,
	},
};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	queue: (Sender<Pending>, Receiver<Pending>),
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			queue: unbounded(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.accept_worker().await;

		Ok(())
	}

	async fn interrupt(&self) {
		let (sender, _) = &self.queue;

		if !sender.is_closed() {
			sender.close();
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
