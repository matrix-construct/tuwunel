pub mod actual;
pub mod cache;
mod dns;
pub mod fed;
#[cfg(test)]
mod tests;
mod well_known;

use std::sync::Arc;

use async_trait::async_trait;
use tuwunel_core::{Result, Server, arrayvec::ArrayString, utils::MutexMap};

use self::{cache::Cache, dns::Resolver};
use crate::{Dep, client};

pub struct Service {
	pub cache: Arc<Cache>,
	pub resolver: Arc<Resolver>,
	resolving: Resolving,
	services: Services,
}

struct Services {
	server: Arc<Server>,
	client: Dep<client::Service>,
}

type Resolving = MutexMap<NameBuf, ()>;
type NameBuf = ArrayString<256>;

#[async_trait]
impl crate::Service for Service {
	#[allow(clippy::as_conversions, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let cache = Cache::new(&args);
		Ok(Arc::new(Self {
			cache: cache.clone(),
			resolver: Resolver::build(args.server, cache)?,
			resolving: MutexMap::new(),
			services: Services {
				server: args.server.clone(),
				client: args.depend::<client::Service>("client"),
			},
		}))
	}

	async fn clear_cache(&self) {
		self.resolver.clear_cache();
		self.cache.clear().await;
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}
