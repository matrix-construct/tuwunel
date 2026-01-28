#![recursion_limit = "256"]
#![type_length_limit = "98304"]
#![expect(refining_impl_trait)]
#![expect(clippy::duration_suboptimal_units)] // remove after MSRV 1.91

mod manager;
mod migrations;
mod once_services;
mod service;
pub mod services;

pub mod account_data;
pub mod admin;
pub mod appservice;
pub mod client;
pub mod config;
pub mod deactivate;
pub mod emergency;
pub mod federation;
pub mod globals;
pub mod key_backups;
pub mod media;
pub mod membership;
pub mod oauth;
pub mod presence;
pub mod pusher;
pub mod registration_tokens;
pub mod resolver;
pub mod rooms;
pub mod sending;
pub mod server_keys;
pub mod sync;
pub mod transaction_ids;
pub mod uiaa;
pub mod users;

pub(crate) use once_services::OnceServices;
pub(crate) use service::{Args, Service};

pub(crate) type SelfServices = std::sync::Arc<OnceServices>;

use log as _;

pub use crate::services::Services;

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}
tuwunel_core::rustc_flags_capture! {}
