//! Synapse admin API: miscellaneous endpoints.

mod fetch_event;
mod scheduled_tasks;
mod server_version;

pub(crate) use self::{
	fetch_event::admin_fetch_event_route, scheduled_tasks::admin_scheduled_tasks_route,
	server_version::admin_server_version_route,
};
