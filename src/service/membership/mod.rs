mod ban;
mod invite;
mod join;
mod kick;
mod knock;
mod leave;
mod unban;

use std::sync::Arc;

use tuwunel_core::Result;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
