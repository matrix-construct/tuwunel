mod append;
mod namespace_regex;
mod ping;
mod registration_info;
pub(crate) mod request;
#[cfg(test)]
mod tests;
mod thirdparty;

#[cfg(unix)]
use std::sync::Mutex;
use std::{
	collections::{BTreeMap, btree_map::Values},
	ffi::OsStr,
	fs::{self, read_dir},
	iter::Filter,
	sync::Arc,
};

use async_trait::async_trait;
use futures::{FutureExt, Stream, TryStreamExt};
use reqwest::Client;
use ruma::{RoomAliasId, RoomId, UserId, api::appservice::Registration};
use serde_yaml::{from_slice as from_yaml, to_string as to_yaml};
use tokio::sync::{RwLock, RwLockReadGuard, SetOnce};
#[cfg(unix)]
use tuwunel_core::smallstr::SmallString;
use tuwunel_core::{
	Err, Result, defer, err, implement,
	utils::{BoolExt, stream::IterStream},
	warn,
};
use tuwunel_database::Map;

pub use self::{
	namespace_regex::NamespaceRegex,
	registration_info::{IS_JOINED_SCOPE, RegistrationInfo},
};
#[cfg(unix)]
use crate::client::make_appservice_unix;

#[cfg(unix)]
type SocketPath = SmallString<[u8; 64]>;

pub struct Service {
	registration_info: RwLock<Registrations>,
	/// Live client pools indexed by socket path.
	///
	/// Entries persist for the service lifetime, including after unregistration.
	#[cfg(unix)]
	unix_clients: Mutex<BTreeMap<SocketPath, Client>>,
	loaded: SetOnce<()>,
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	id_appserviceregistrations: Arc<Map>,
}

type Registrations = BTreeMap<String, RegistrationInfo>;

const KNOWN_SCOPES: [&str; 1] = [IS_JOINED_SCOPE];

/// The registrations [`receivers`] yields.
///
/// A named type rather than an opaque one, so a caller may shorten its borrow
/// of the registry.
pub type Receivers<'a> =
	Filter<Values<'a, String, RegistrationInfo>, fn(&&RegistrationInfo) -> bool>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			registration_info: RwLock::new(BTreeMap::new()),
			#[cfg(unix)]
			unix_clients: Mutex::default(),
			loaded: SetOnce::new(),
			services: args.services.clone(),
			db: Data {
				id_appserviceregistrations: args.db["id_appserviceregistrations"].clone(),
			},
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		defer! {{
			self.loaded.set(()).ok();
		}}

		self.load().await
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Loads every registration source into the runtime registry.
	///
	/// The configured `appservice` table is read first, then any YAML under
	/// `appservice_dir`, then the registrations persisted by the admin command.
	async fn load(&self) -> Result {
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
			let entries = read_dir(appservice_dir).map_err(|e| {
				err!(Config("appservice_dir", "Failed to read {appservice_dir:?}: {e}"))
			})?;

			for dir_entry in entries {
				let path = dir_entry?.path();

				if !path.is_file()
					|| !path
						.extension()
						.and_then(OsStr::to_str)
						.is_some_and(|ext| matches!(ext, "yaml" | "yml"))
				{
					continue;
				}

				let bytes = fs::read(path)?;
				let registration: Registration = from_yaml(&bytes)?;

				self.load_appservice(registration).await?;
			}
		}

		self.iter_db_ids()
			.try_for_each(|registration| self.load_appservice(registration))
			.await?;

		Ok(())
	}

	/// Loads one registration into the runtime registry at startup.
	///
	/// An id already loaded is refused, as is an `as_token` another id holds,
	/// and a missing sender user is created. An unknown scope is logged and
	/// ignored, so a registration written for a later release still loads.
	pub async fn load_appservice(&self, registration: Registration) -> Result {
		let registration_info =
			RegistrationInfo::new(registration, self.services.globals.server_name())?;

		let id = &registration_info.registration.id;

		if let Some(scope) = registration_info.unknown_scope() {
			warn!(%id, %scope, "Ignoring an unknown scope in the appservice registration");
		}

		let mut registrations = self.registration_info.write().await;

		if registrations.contains_key(id) {
			return Err!("Duplicate id: {id}");
		}

		self.admit_registration(&registrations, &registration_info)
			.await?;

		registrations.insert(id.clone(), registration_info);

		Ok(())
	}

	/// Registers an appservice and persists it for later startups.
	///
	/// An id this method registered before is replaced, and its cached room
	/// participation is dropped. An id loaded from the configuration or
	/// `appservice_dir` is refused, since both would load at the next startup
	/// and collide. A scope this server does not know is refused too.
	pub async fn register_appservice(&self, registration: Registration) -> Result {
		self.loaded().await;

		let id = registration.id.clone();

		let yaml = to_yaml(&registration)?;

		let registration_info =
			RegistrationInfo::new(registration, self.services.globals.server_name())?;

		if let Some(scope) = registration_info.unknown_scope() {
			return Err!("Unknown scope {scope:?} in appservice {id}");
		}

		let mut registrations = self.registration_info.write().await;

		if registrations.contains_key(&id) && self.is_persisted(&id).await.is_false() {
			return Err!("Cannot replace config appservice {id}");
		}

		self.admit_registration(&registrations, &registration_info)
			.await?;

		self.db
			.id_appserviceregistrations
			.insert(&id, yaml);

		// Room participation lookups run under a registry read guard, so none
		// built on the replaced registration outlives this write guard.
		self.services
			.state_cache
			.clear_appservice_in_room_cache_for(&id);

		registrations.insert(id, registration_info);

		Ok(())
	}

	pub async fn unregister_appservice(&self, appservice_id: &str) -> Result {
		self.loaded().await;

		let mut registrations = self.registration_info.write().await;

		if !registrations.contains_key(appservice_id) {
			return Err!("Appservice not found");
		}

		if self.is_persisted(appservice_id).await.is_false() {
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

	/// Retrieve a registration with its compiled namespaces (`sender`,
	/// `is_user_match`), which a bare `Registration` lacks.
	pub async fn get_registration_info(&self, id: &str) -> Option<RegistrationInfo> {
		self.registration_info
			.read()
			.await
			.get(id)
			.cloned()
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

	/// Checks if a given user id matches any appservice's user namespace.
	pub async fn is_interested_in_user(&self, user_id: &UserId) -> bool {
		self.read()
			.await
			.values()
			.any(|info| info.is_user_match(user_id))
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
			.and_then(|ref bytes| from_yaml(bytes).map_err(Into::into))
			.map_err(|e| err!(Database("Invalid appservice {id:?} registration: {e:?}")))
	}

	pub fn read(&self) -> impl Future<Output = RwLockReadGuard<'_, Registrations>> + Send {
		self.registration_info.read()
	}

	/// Waits for the boot-time registration load to finish.
	///
	/// The latch is released on every exit from the worker, a failed or
	/// panicking load included, so a waiter is never stranded on a load that
	/// will not complete.
	#[inline]
	pub async fn loaded(&self) { self.loaded.wait().await; }
}

/// The registrations a sender may queue traffic for.
///
/// A registration without a URL asked for no traffic, so it is left out.
pub fn receivers(registrations: &Registrations) -> Receivers<'_> {
	let has_destination: fn(&&RegistrationInfo) -> bool = |info| info.has_destination();

	registrations.values().filter(has_destination)
}

/// MSC4502: the first requested scope this server does not know.
///
/// Such a scope grants nothing, since `has_scope` is only asked about known
/// ones; it is most likely a typo the operator should hear about.
#[implement(RegistrationInfo)]
#[must_use]
pub fn unknown_scope(&self) -> Option<&str> {
	self.registration
		.scopes
		.iter()
		.map(String::as_str)
		.find(|scope| !KNOWN_SCOPES.contains(scope))
}

/// Refuses an `as_token` another id holds, and creates a missing sender user.
///
/// The caller inserts the registration under the same write guard.
#[implement(Service)]
async fn admit_registration(
	&self,
	registrations: &Registrations,
	registration_info: &RegistrationInfo,
) -> Result {
	//TODO: Check for collisions between exclusive appservice namespaces

	let id = &registration_info.registration.id;
	let token_holder = registrations
		.values()
		.map(|loaded| &loaded.registration)
		.find(|loaded| {
			loaded.id != *id && loaded.as_token == registration_info.registration.as_token
		})
		.map(|holder| &holder.id);

	if let Some(holder) = token_holder {
		return Err!("Duplicate as_token: {holder} {id}");
	}

	let appservice_user = &registration_info.sender;

	if self
		.services
		.users
		.exists(appservice_user)
		.await
		.is_false()
	{
		self.services
			.users
			.create(appservice_user, None, None)
			.await?;
	}

	Ok(())
}

/// Whether `register_appservice` persisted this id, which an id loaded from
/// the configuration or `appservice_dir` never is.
#[implement(Service)]
async fn is_persisted(&self, id: &str) -> bool {
	self.db
		.id_appserviceregistrations
		.exists(id)
		.await
		.is_ok()
}

#[implement(Service)]
fn transport<'a>(&self, url: &'a str) -> Result<(&'a str, Client)> {
	match socket_path(url)? {
		| None => Ok((url, self.services.client.appservice.clone())),
		#[cfg(not(unix))]
		| Some(_) => Err!("Unix sockets are not supported on this platform"),
		#[cfg(unix)]
		| Some(path) => Ok(("http://localhost", self.unix_client(path)?)),
	}
}

#[cfg(unix)]
#[implement(Service)]
fn unix_client(&self, path: &str) -> Result<Client> {
	let mut clients = self.unix_clients.lock().expect("locked");

	match clients.get(path) {
		| Some(client) => Ok(client.clone()),
		| None => {
			let client = make_appservice_unix(&self.services, path)?;

			clients.insert(path.into(), client.clone());
			Ok(client)
		},
	}
}

/// The registration's URL, absent when it asks for no traffic.
///
/// A `null` or empty URL is taken as absent too.
fn destination(registration: &Registration) -> Option<&str> {
	registration
		.url
		.as_deref()
		.filter(|url| !url.is_empty() && *url != "null")
}

fn socket_path(url: &str) -> Result<Option<&str>> {
	url.split_once(':')
		.filter(|(scheme, _)| scheme.eq_ignore_ascii_case("unix"))
		.map(|(_, path)| path.strip_prefix("//").unwrap_or(path))
		.map(|path| {
			path.starts_with('/')
				.then_some(path)
				.ok_or_else(|| err!("Appservice Unix socket URL must name an absolute path"))
		})
		.transpose()
}
