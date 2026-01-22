mod append;
mod namespace_regex;
mod registration_info;
pub(crate) mod request;

use std::{
	collections::{BTreeMap, HashSet},
	iter::IntoIterator,
	sync::Arc,
};

use async_trait::async_trait;
use futures::{Future, FutureExt, Stream, StreamExt, TryStreamExt};
use ruma::{RoomAliasId, RoomId, UserId, api::appservice::Registration};
use tokio::sync::{RwLock, RwLockReadGuard};
use tuwunel_core::{Err, Result, debug, err, utils::stream::IterStream};
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
		self.init_registrations().await?;
		self.check_registrations().await?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	#[tracing::instrument(name = "init", skip(self))]
	async fn init_registrations(&self) -> Result {
		// Registrations from configuration file
		let confs = self
			.services
			.server
			.config
			.appservice
			.clone()
			.into_iter()
			.stream()
			.map(|(id, mut reg)| {
				reg.id.clone_from(&id);
				reg.sender_localpart
					.get_or_insert_with(|| id.clone());

				Ok((id, reg))
			});

		// Registrations from database
		self.iter_db_ids()
			.chain(confs.map_ok(|(id, reg)| (id, reg.into())))
			.try_for_each(async |(id, reg): (_, Registration)| {
				debug!(?id, ?reg, "appservice registration");
				self.registration_info
					.write()
					.await
					.insert(id.clone(), reg.try_into()?)
					.map_or(Ok(()), |_| Err!("Conflicting Appservice ID: {id:?}"))
			})
			.await
	}

	#[tracing::instrument(name = "check", skip(self))]
	async fn check_registrations(&self) -> Result {
		let regs = self.registration_info.read().await;

		let num_as_tokens = regs
			.values()
			.map(|info| &info.registration.as_token)
			.collect::<HashSet<_>>()
			.len();

		if num_as_tokens < regs.len() {
			return Err!(
				"Conflicting Appservice registrations: each must have a unique as_token."
			);
		}

		Ok(())
	}

	/// Registers an appservice and returns the ID to the caller
	pub async fn register_appservice(
		&self,
		registration: &Registration,
		appservice_config_body: &str,
	) -> Result {
		let appservice_user = UserId::parse_with_server_name(
			&registration.sender_localpart,
			&self.services.config.server_name,
		)?;

		if !self.services.users.exists(&appservice_user).await {
			self.services
				.users
				.create(&appservice_user, None, None)
				.await?;
		}

		//TODO: Check for collisions between exclusive appservice namespaces
		self.registration_info
			.write()
			.await
			.insert(registration.id.clone(), registration.clone().try_into()?);

		self.db
			.id_appserviceregistrations
			.insert(&registration.id, appservice_config_body);

		Ok(())
	}

	/// Remove an appservice registration
	///
	/// # Arguments
	///
	/// * `service_name` - the registration ID of the appservice
	pub async fn unregister_appservice(&self, appservice_id: &str) -> Result {
		// removes the appservice registration info
		self.registration_info
			.write()
			.await
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

	pub fn iter_db_ids(&self) -> impl Stream<Item = Result<(String, Registration)>> + Send {
		self.db
			.id_appserviceregistrations
			.keys()
			.and_then(async move |id: &str| {
				Ok((id.to_owned(), self.get_db_registration(id).await?))
			})
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
