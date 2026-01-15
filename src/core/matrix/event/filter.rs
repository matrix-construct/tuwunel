use ruma::{
	RoomId, UserId,
	api::client::filter::{Filter, RoomEventFilter, RoomFilter, UrlFilter},
};
use serde_json::Value;

use super::Event;
use crate::is_equal_to;

pub trait Matches<T> {
	fn matches(&self, t: T) -> bool;
}

impl<E: Event> Matches<&E> for RoomEventFilter {
	#[inline]
	fn matches(&self, event: &E) -> bool {
		if !matches_sender(event, self) {
			return false;
		}

		if !matches_room(event, self) {
			return false;
		}

		if !matches_type(event, self) {
			return false;
		}

		if !matches_url(event, self) {
			return false;
		}

		true
	}
}

impl Matches<&RoomId> for RoomFilter {
	#[inline]
	fn matches(&self, room_id: &RoomId) -> bool {
		if !matches_room_id(room_id, self) {
			return false;
		}

		true
	}
}

impl Matches<&UserId> for Filter {
	#[inline]
	fn matches(&self, user_id: &UserId) -> bool {
		if !matches_user_id(user_id, self) {
			return false;
		}

		true
	}
}

fn matches_user_id(user_id: &UserId, filter: &Filter) -> bool {
	if filter
		.not_senders
		.iter()
		.any(is_equal_to!(user_id))
	{
		return false;
	}

	if let Some(senders) = filter.senders.as_ref()
		&& !senders.iter().any(is_equal_to!(user_id))
	{
		return false;
	}

	true
}

fn matches_room_id(room_id: &RoomId, filter: &RoomFilter) -> bool {
	if filter.not_rooms.iter().any(is_equal_to!(room_id)) {
		return false;
	}

	if let Some(rooms) = filter.rooms.as_ref()
		&& !rooms.iter().any(is_equal_to!(room_id))
	{
		return false;
	}

	true
}

fn matches_room<E: Event>(event: &E, filter: &RoomEventFilter) -> bool {
	if filter
		.not_rooms
		.iter()
		.any(is_equal_to!(event.room_id()))
	{
		return false;
	}

	if let Some(rooms) = filter.rooms.as_ref()
		&& !rooms.iter().any(is_equal_to!(event.room_id()))
	{
		return false;
	}

	true
}

fn matches_sender<E: Event>(event: &E, filter: &RoomEventFilter) -> bool {
	if filter
		.not_senders
		.iter()
		.any(is_equal_to!(event.sender()))
	{
		return false;
	}

	if let Some(senders) = filter.senders.as_ref()
		&& !senders.iter().any(is_equal_to!(event.sender()))
	{
		return false;
	}

	true
}

fn matches_type<E: Event>(event: &E, filter: &RoomEventFilter) -> bool {
	let kind = event.kind().to_cow_str();

	if filter.not_types.iter().any(is_equal_to!(&kind)) {
		return false;
	}

	if let Some(types) = filter.types.as_ref()
		&& !types.iter().any(is_equal_to!(&kind))
	{
		return false;
	}

	true
}

fn matches_url<E: Event>(event: &E, filter: &RoomEventFilter) -> bool {
	let Some(url_filter) = filter.url_filter.as_ref() else {
		return true;
	};

	//TODO: might be better to use Ruma's Raw rather than serde here
	let url = event
		.get_content_as_value()
		.get("url")
		.is_some_and(Value::is_string);

	match url_filter {
		| UrlFilter::EventsWithUrl => url,
		| UrlFilter::EventsWithoutUrl => !url,
	}
}
