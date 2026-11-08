mod args;
pub(crate) mod auth;
mod client_ip;
mod handler;
mod html;
mod request;
mod response;
pub mod state;

pub use client_ip::{ConfiguredIpSource, TrustedPeerSubnets};

#[cfg(feature = "jwt_login")]
pub use self::auth::jwt::validate_user;
pub use self::{
	args::{Args as Ruma, ArgsAdmin as RumaAdmin},
	auth::{admin::require_admin, auth_uiaa},
	client_ip::{ClientIp, RateLimitIp},
	handler::{RouterExt, RumaHandler},
	html::ACCOUNT_HEAD,
	request::read_body,
	response::RumaResponse,
	state::State,
};
