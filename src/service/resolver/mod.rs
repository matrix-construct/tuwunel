pub mod actual;
pub mod cache;
mod dns;
pub mod fed;
#[cfg(test)]
mod tests;
mod well_known;

use std::sync::Arc;

use async_trait::async_trait;
use ruma::OwnedServerName;
use tuwunel_core::{Result, smallstr::SmallString, utils::MutexMap};

use self::{cache::Cache, dns::Resolver, fed::FedDest};

pub struct Service {
	pub cache: Arc<Cache>,
	pub resolver: Arc<Resolver>,
	resolving: Resolving,
	services: Arc<crate::services::OnceServices>,
}

pub(crate) type DestString = SmallString<[u8; 40]>;
type Resolving = MutexMap<OwnedServerName, ()>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let cache = Cache::new(args);
		Ok(Arc::new(Self {
			cache: cache.clone(),
			resolver: Resolver::build(args.server, cache)?,
			resolving: MutexMap::new(),
			services: args.services.clone(),
		}))
	}

	async fn clear_cache(&self) {
		self.resolver.clear_cache();
		self.cache.clear().await;
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
