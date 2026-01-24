use std::{fmt, sync::Arc};

use futures::{StreamExt, TryStreamExt};
use tokio::sync::Mutex;
use tuwunel_core::{
	Result, Server, debug, debug_info, implement, info, trace, utils::stream::IterStream,
};
use tuwunel_database::Database;

pub(crate) use crate::OnceServices;
use crate::{
	account_data, admin, appservice, client, config, deactivate, emergency, federation, globals,
	key_backups,
	manager::Manager,
	media, membership, oauth, presence, pusher, registration_tokens, resolver,
	rooms::{self, retention},
	sending, server_keys,
	service::{Args, Service},
	sync, transaction_ids, uiaa, users,
};

pub struct Services {
	pub account_data: Arc<account_data::Service>,
	pub admin: Arc<admin::Service>,
	pub appservice: Arc<appservice::Service>,
	pub config: Arc<config::Service>,
	pub client: Arc<client::Service>,
	pub emergency: Arc<emergency::Service>,
	pub globals: Arc<globals::Service>,
	pub key_backups: Arc<key_backups::Service>,
	pub media: Arc<media::Service>,
	pub presence: Arc<presence::Service>,
	pub pusher: Arc<pusher::Service>,
	pub resolver: Arc<resolver::Service>,
	pub alias: Arc<rooms::alias::Service>,
	pub auth_chain: Arc<rooms::auth_chain::Service>,
	pub delete: Arc<rooms::delete::Service>,
	pub directory: Arc<rooms::directory::Service>,
	pub event_handler: Arc<rooms::event_handler::Service>,
	pub lazy_loading: Arc<rooms::lazy_loading::Service>,
	pub metadata: Arc<rooms::metadata::Service>,
	pub pdu_metadata: Arc<rooms::pdu_metadata::Service>,
	pub read_receipt: Arc<rooms::read_receipt::Service>,
	pub search: Arc<rooms::search::Service>,
	pub short: Arc<rooms::short::Service>,
	pub spaces: Arc<rooms::spaces::Service>,
	pub state: Arc<rooms::state::Service>,
	pub state_accessor: Arc<rooms::state_accessor::Service>,
	pub state_cache: Arc<rooms::state_cache::Service>,
	pub state_compressor: Arc<rooms::state_compressor::Service>,
	pub threads: Arc<rooms::threads::Service>,
	pub timeline: Arc<rooms::timeline::Service>,
	pub typing: Arc<rooms::typing::Service>,
	pub federation: Arc<federation::Service>,
	pub sending: Arc<sending::Service>,
	pub server_keys: Arc<server_keys::Service>,
	pub sync: Arc<sync::Service>,
	pub transaction_ids: Arc<transaction_ids::Service>,
	pub uiaa: Arc<uiaa::Service>,
	pub users: Arc<users::Service>,
	pub membership: Arc<membership::Service>,
	pub deactivate: Arc<deactivate::Service>,
	pub oauth: Arc<oauth::Service>,
	pub retention: Arc<retention::Service>,
	pub registration_tokens: Arc<registration_tokens::Service>,

	manager: Mutex<Option<Arc<Manager>>>,
	pub server: Arc<Server>,
	pub db: Arc<Database>,
}

#[implement(Services)]
pub async fn build(server: Arc<Server>) -> Result<Arc<Self>> {
	let db = Database::open(&server).await?;
	let services = Arc::new(OnceServices::default());
	let args = Args {
		db: &db,
		server: &server,
		services: &services,
	};

	let res = Arc::new(Self {
		account_data: account_data::Service::build(&args)?,
		admin: admin::Service::build(&args)?,
		appservice: appservice::Service::build(&args)?,
		resolver: resolver::Service::build(&args)?,
		client: client::Service::build(&args)?,
		config: config::Service::build(&args)?,
		emergency: emergency::Service::build(&args)?,
		globals: globals::Service::build(&args)?,
		key_backups: key_backups::Service::build(&args)?,
		media: media::Service::build(&args)?,
		presence: presence::Service::build(&args)?,
		pusher: pusher::Service::build(&args)?,
		alias: rooms::alias::Service::build(&args)?,
		auth_chain: rooms::auth_chain::Service::build(&args)?,
		delete: rooms::delete::Service::build(&args)?,
		directory: rooms::directory::Service::build(&args)?,
		event_handler: rooms::event_handler::Service::build(&args)?,
		lazy_loading: rooms::lazy_loading::Service::build(&args)?,
		metadata: rooms::metadata::Service::build(&args)?,
		pdu_metadata: rooms::pdu_metadata::Service::build(&args)?,
		read_receipt: rooms::read_receipt::Service::build(&args)?,
		search: rooms::search::Service::build(&args)?,
		short: rooms::short::Service::build(&args)?,
		spaces: rooms::spaces::Service::build(&args)?,
		state: rooms::state::Service::build(&args)?,
		state_accessor: rooms::state_accessor::Service::build(&args)?,
		state_cache: rooms::state_cache::Service::build(&args)?,
		state_compressor: rooms::state_compressor::Service::build(&args)?,
		threads: rooms::threads::Service::build(&args)?,
		timeline: rooms::timeline::Service::build(&args)?,
		typing: rooms::typing::Service::build(&args)?,
		federation: federation::Service::build(&args)?,
		sending: sending::Service::build(&args)?,
		server_keys: server_keys::Service::build(&args)?,
		sync: sync::Service::build(&args)?,
		transaction_ids: transaction_ids::Service::build(&args)?,
		uiaa: uiaa::Service::build(&args)?,
		users: users::Service::build(&args)?,
		membership: membership::Service::build(&args)?,
		deactivate: deactivate::Service::build(&args)?,
		oauth: oauth::Service::build(&args)?,
		retention: retention::Service::build(&args)?,
		registration_tokens: registration_tokens::Service::build(&args)?,

		manager: Mutex::new(None),
		server,
		db,
	});

	Ok(services.set(res))
}

#[implement(Services)]
pub(crate) fn services(&self) -> impl Iterator<Item = Arc<dyn Service>> + Send {
	macro_rules! cast {
		($s:expr) => {
			<Arc<dyn Service> as Into<_>>::into($s.clone())
		};
	}

	[
		cast!(self.account_data),
		cast!(self.admin),
		cast!(self.appservice),
		cast!(self.resolver),
		cast!(self.client),
		cast!(self.config),
		cast!(self.emergency),
		cast!(self.globals),
		cast!(self.key_backups),
		cast!(self.media),
		cast!(self.presence),
		cast!(self.pusher),
		cast!(self.alias),
		cast!(self.auth_chain),
		cast!(self.delete),
		cast!(self.directory),
		cast!(self.event_handler),
		cast!(self.lazy_loading),
		cast!(self.metadata),
		cast!(self.pdu_metadata),
		cast!(self.read_receipt),
		cast!(self.search),
		cast!(self.short),
		cast!(self.spaces),
		cast!(self.state),
		cast!(self.state_accessor),
		cast!(self.state_cache),
		cast!(self.state_compressor),
		cast!(self.threads),
		cast!(self.timeline),
		cast!(self.typing),
		cast!(self.federation),
		cast!(self.sending),
		cast!(self.server_keys),
		cast!(self.sync),
		cast!(self.transaction_ids),
		cast!(self.uiaa),
		cast!(self.users),
		cast!(self.membership),
		cast!(self.deactivate),
		cast!(self.oauth),
		cast!(self.retention),
		cast!(self.registration_tokens),
	]
	.into_iter()
}

impl fmt::Debug for Services {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Services").finish()
	}
}

#[implement(Services)]
pub async fn start(self: &Arc<Self>) -> Result<Arc<Self>> {
	debug_info!("Starting services...");

	super::migrations::migrations(self).await?;
	self.manager
		.lock()
		.await
		.insert(Manager::new(self))
		.clone()
		.start()
		.await?;

	debug_info!("Services startup complete.");

	Ok(Arc::clone(self))
}

#[implement(Services)]
pub async fn stop(&self) {
	info!("Shutting down services...");

	self.interrupt().await;
	if let Some(manager) = self.manager.lock().await.as_ref() {
		manager.stop().await;
	}

	debug_info!("Services shutdown complete.");
}

#[implement(Services)]
pub(crate) async fn interrupt(&self) {
	debug!("Interrupting services...");
	for service in self.services() {
		let name = service.name();
		trace!("Interrupting {name}");
		service.interrupt().await;
	}
}

#[implement(Services)]
pub async fn poll(&self) -> Result {
	if let Some(manager) = self.manager.lock().await.as_ref() {
		return manager.poll().await;
	}

	Ok(())
}

#[implement(Services)]
pub async fn clear_cache(&self) {
	self.services()
		.stream()
		.for_each(async |service| {
			service.clear_cache().await;
		})
		.await;
}

#[implement(Services)]
pub async fn memory_usage(&self) -> Result<String> {
	self.services()
		.try_stream()
		.try_fold(String::new(), async |mut out, service| {
			service.memory_usage(&mut out).await?;
			Ok(out)
		})
		.await
}
