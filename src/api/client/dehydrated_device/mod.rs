mod delete_dehydrated_device;
mod get_dehydrated_device;
mod get_dehydrated_events;
mod put_dehydrated_device;

pub(crate) use self::{
	delete_dehydrated_device::delete_dehydrated_device_route,
	get_dehydrated_device::get_dehydrated_device_route,
	get_dehydrated_events::get_dehydrated_events_route,
	put_dehydrated_device::put_dehydrated_device_route,
};
