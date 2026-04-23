mod append;
mod namespace_regex;
mod registration_info;
pub(crate) mod request;

use std::{collections::BTreeMap, fs, iter::IntoIterator, sync::Arc};

use async_trait::async_trait;
use futures::{Future, FutureExt, Stream, TryStreamExt};
use ruma::{RoomAliasId, RoomId, UserId, api::appservice::Registration};
use tokio::sync::{RwLock, RwLockReadGuard};
use tuwunel_core::{Err, Result, err, utils::stream::IterStream};
use tuwunel_database::Map;

pub use self::{namespace_regex::NamespaceRegex, registration_info::RegistrationInfo};

pub struct Service {
	registration_info: RwLock<Registrations>,
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	id_appserviceregistrations: Arc<Map>,
}

type Registrations = BTreeMap<String, RegistrationInfo>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			registration_info: RwLock::new(BTreeMap::new()),
			services: args.services.clone(),
			db: Data {
				id_appserviceregistrations: args.db["id_appserviceregistrations"].clone(),
			},
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		for (id, mut appservice) in self.services.config.appservice.clone() {
			if appservice.id.is_empty() {
				appservice.id = id.clone();
			}

			if *id != appservice.id {
				return Err!(Config(
					"id",
					"Registration ID {:?} does not match the configured {id:?}",
					appservice.id
				));
			}

			self.load_appservice(appservice.into()).await?;
		}

		if let Some(appservice_dir) = &self.services.config.appservice_dir {
			for dir_entry in fs::read_dir(appservice_dir)? {
				let path = dir_entry?.path();
				let bytes = fs::read(path)?;
				let registration: Registration = serde_yaml::from_slice(&bytes)?;

				self.load_appservice(registration).await?;
			}
		}

		self.iter_db_ids()
			.try_for_each(|registration| self.load_appservice(registration))
			.await?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub async fn load_appservice(&self, registration: Registration) -> Result {
		//TODO: Check for collisions between exclusive appservice namespaces

		let registration_info =
			RegistrationInfo::new(registration, self.services.globals.server_name())?;

		let id = &registration_info.registration.id;

		let mut registrations = self.registration_info.write().await;

		for loaded_registration_info in registrations.values() {
			let loaded_id = &loaded_registration_info.registration.id;

			if loaded_id == id {
				return Err!("Duplicate id: {id}");
			}

			if loaded_registration_info.registration.as_token
				== registration_info.registration.as_token
			{
				return Err!("Duplicate as_token: {loaded_id} {id}");
			}
		}

		let appservice_user = &registration_info.sender;

		if !self.services.users.exists(appservice_user).await {
			self.services
				.users
				.create(appservice_user, None, None)
				.await?;
		}

		registrations.insert(id.clone(), registration_info);

		Ok(())
	}

	pub async fn register_appservice(&self, registration: Registration) -> Result {
		let id = registration.id.clone();

		let appservice_yaml = serde_yaml::to_string(&registration)?;

		self.load_appservice(registration).await?;

		self.db
			.id_appserviceregistrations
			.insert(&id, appservice_yaml);

		Ok(())
	}

	pub async fn unregister_appservice(&self, appservice_id: &str) -> Result {
		let mut registrations = self.registration_info.write().await;

		if !registrations.contains_key(appservice_id) {
			return Err!("Appservice not found");
		}

		if self
			.db
			.id_appserviceregistrations
			.exists(appservice_id)
			.await
			.is_err()
		{
			return Err!("Cannot unregister config appservice");
		}

		// removes the appservice registration info
		registrations
			.remove(appservice_id)
			.ok_or_else(|| err!("Appservice not found"))?;

		// remove the appservice from the database
		self.db
			.id_appserviceregistrations
			.remove(appservice_id);

		// deletes all active requests for the appservice if there are any so we stop
		// sending to the URL
		self.services
			.sending
			.cleanup_events(Some(appservice_id), None, None)
			.await
	}

	pub async fn get_registration(&self, id: &str) -> Option<Registration> {
		self.registration_info
			.read()
			.await
			.get(id)
			.cloned()
			.map(|info| info.registration)
	}

	pub async fn find_from_access_token(&self, token: &str) -> Result<RegistrationInfo> {
		self.read()
			.await
			.values()
			.find(|info| info.registration.as_token == token)
			.cloned()
			.ok_or_else(|| err!(Request(NotFound("Missing or invalid appservice token"))))
	}

	/// Checks if a given user id matches any exclusive appservice regex
	pub async fn is_exclusive_user_id(&self, user_id: &UserId) -> bool {
		self.read()
			.await
			.values()
			.any(|info| info.is_exclusive_user_match(user_id))
	}

	/// Checks if a given room alias matches any exclusive appservice regex
	pub async fn is_exclusive_alias(&self, alias: &RoomAliasId) -> bool {
		self.read()
			.await
			.values()
			.any(|info| info.aliases.is_exclusive_match(alias.as_str()))
	}

	/// Checks if a given room id matches any exclusive appservice regex
	///
	/// TODO: use this?
	pub async fn is_exclusive_room_id(&self, room_id: &RoomId) -> bool {
		self.read()
			.await
			.values()
			.any(|info| info.rooms.is_exclusive_match(room_id.as_str()))
	}

	pub fn iter_ids(&self) -> impl Stream<Item = String> + Send {
		self.read()
			.map(|info| info.keys().cloned().collect::<Vec<_>>())
			.map(IntoIterator::into_iter)
			.map(IterStream::stream)
			.flatten_stream()
	}

	pub fn iter_db_ids(&self) -> impl Stream<Item = Result<Registration>> + Send {
		self.db
			.id_appserviceregistrations
			.keys()
			.and_then(async move |id: &str| Ok(self.get_db_registration(id).await?))
	}

	pub async fn get_db_registration(&self, id: &str) -> Result<Registration> {
		self.db
			.id_appserviceregistrations
			.get(id)
			.await
			.and_then(|ref bytes| serde_yaml::from_slice(bytes).map_err(Into::into))
			.map_err(|e| err!(Database("Invalid appservice {id:?} registration: {e:?}")))
	}

	pub fn read(&self) -> impl Future<Output = RwLockReadGuard<'_, Registrations>> + Send {
		self.registration_info.read()
	}
}
