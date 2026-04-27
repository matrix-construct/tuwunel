#![expect(clippy::too_many_arguments)]

pub(crate) mod admin;
pub(crate) mod context;
pub(crate) mod processor;
mod tests;
pub(crate) mod utils;

pub(crate) mod appservice;
pub(crate) mod debug;
pub(crate) mod federation;
pub(crate) mod media;
pub(crate) mod query;
pub(crate) mod room;
pub(crate) mod server;
pub(crate) mod token;
pub(crate) mod user;

pub(crate) use tuwunel_macros::{admin_command, admin_command_dispatch};

pub(crate) use crate::{context::Context, utils::get_room_info};

pub(crate) const PAGE_SIZE: usize = 100;

use log as _;

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
tuwunel_core::rustc_flags_capture! {}

/// Install the admin command processor
pub async fn init(admin_service: &tuwunel_service::admin::Service) {
	_ = admin_service
		.complete
		.write()
		.expect("locked for writing")
		.insert(processor::complete);
	_ = admin_service
		.handle
		.write()
		.await
		.insert(processor::dispatch);
}

/// Uninstall the admin command handler
pub async fn fini(admin_service: &tuwunel_service::admin::Service) {
	_ = admin_service.handle.write().await.take();
	_ = admin_service
		.complete
		.write()
		.expect("locked for writing")
		.take();
}
