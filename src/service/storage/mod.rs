pub mod provider;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use futures::TryStreamExt;
pub use object_store::{CopyMode, GetResult, GetResultPayload, PutPayload, PutResult};
use tuwunel_core::{
	Result, at,
	config::{StorageProvider, StorageProviderLocal},
	derivative::Derivative,
	err, implement,
	utils::{BoolExt, stream::IterStream},
};

pub use self::provider::Provider;

#[derive(Derivative)]
#[derivative(Debug)]
pub struct Service {
	providers: Providers,

	#[derivative(Debug = "ignore")]
	services: Arc<crate::services::OnceServices>,
}

type Providers = BTreeMap<String, Arc<Provider>>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			providers: Self::build_providers(args)?,
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.start_providers().await?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
#[tracing::instrument(
	level = "info",
	err(level = "error")
	skip_all,
)]
fn build_providers(args: &crate::Args<'_>) -> Result<Providers> {
	let default_media_provider = args
		.server
		.config
		.storage_provider
		.contains_key("media")
		.is_false()
		.then(|| {
			let db_path = args.server.config.database_path.clone();
			let provider = StorageProviderLocal {
				create_if_missing: true,
				base_path: [db_path, "media".into()]
					.into_iter()
					.collect::<PathBuf>()
					.to_string_lossy()
					.into(),

				..Default::default()
			};

			("media".into(), StorageProvider::local(provider))
		});

	args.server
		.config
		.storage_provider
		.iter()
		.chain(
			default_media_provider
				.iter()
				.map(|(name, conf)| (name, conf)),
		)
		.filter_map(|(name, conf)| match conf {
			| StorageProvider::local(conf) => provider::local::new(args, name, conf).transpose(),
			| StorageProvider::s3(conf) => provider::s3::new(args, name, conf).transpose(),
			| _ => None,
		})
		.collect::<Result<_>>()
}

#[implement(Service)]
async fn start_providers(&self) -> Result {
	self.providers
		.iter()
		.map(at!(1))
		.try_stream()
		.and_then(Provider::start)
		.try_collect()
		.await
}

/// Get the specific storage provider's instance by ID.
#[implement(Service)]
pub fn provider<'a>(&'a self, id: &'a str) -> Result<&'a Arc<Provider>> {
	self.providers
		.get(id)
		.ok_or_else(|| err!(Request(NotFound(error!("No instance of provider")))))
}

/// Get the specific storage provider's configuration by ID.
#[implement(Service)]
pub fn config<'a>(&'a self, id: &'a str) -> Result<&'a StorageProvider> {
	self.configs(Some(id))
		.next()
		.map(at!(1))
		.ok_or_else(|| err!(Request(NotFound("No configuration for provider"))))
}

/// Iterate the storage provider configurations.
#[implement(Service)]
pub fn providers(&self) -> impl Iterator<Item = &Arc<Provider>> + Send + '_ {
	self.providers.values()
}

/// Iterate the storage provider configurations.
#[implement(Service)]
pub fn configs<'a, Id>(
	&'a self,
	id: Id,
) -> impl Iterator<Item = (&'a String, &'a StorageProvider)> + Send + 'a
where
	Id: Into<Option<&'a str>>,
{
	let id = id.into();

	self.services
		.config
		.storage_provider
		.iter()
		.filter(move |(id_, _)| id.is_none_or(|id| id_.starts_with(id)))
}
