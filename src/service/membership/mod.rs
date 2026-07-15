mod ban;
mod invite;
mod join;
mod kick;
mod knock;
mod leave;
mod stripped_state;
mod unban;

use std::sync::Arc;

use tuwunel_core::Result;

pub use self::{
	join::Join,
	stripped_state::{
		StrippedCreateVerdict, enforce_stripped_create, into_client_stripped, v12_room_ids,
	},
};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
