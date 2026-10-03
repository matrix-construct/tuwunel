//! Synapse admin API: miscellaneous endpoints.

mod fetch_event;
mod scheduled_tasks;
mod send_server_notice;
mod server_version;

pub(crate) use self::{
	fetch_event::admin_fetch_event_route,
	scheduled_tasks::admin_scheduled_tasks_route,
	send_server_notice::{admin_send_server_notice_route, admin_send_server_notice_txn_route},
	server_version::admin_server_version_route,
};
