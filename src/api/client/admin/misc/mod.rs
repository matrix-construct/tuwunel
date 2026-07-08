//! Synapse admin API: miscellaneous endpoints.

mod fetch_event;
mod server_version;

pub(crate) use self::{
	fetch_event::admin_fetch_event_route, server_version::admin_server_version_route,
};
