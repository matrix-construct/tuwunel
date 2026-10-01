use std::iter::once;

use axum::extract::State;
use futures::{Stream, StreamExt, future::ok, pin_mut};
use ruma::{
	UserId,
	api::client::peeking::listen_to_new_events::v3::{Request, Response},
};
use tokio::time::{Duration, Instant, timeout_at};
use tuwunel_core::{
	Err, Event, Result, err,
	matrix::PduCount,
	utils::{
		BoolExt, OptionExt,
		future::OptionFutureExt,
		result::FlatOk,
		stream::{IterStream, ReadyExt, WidebandExt},
	},
};
use tuwunel_service::{Services, rooms::timeline::PdusIterItem};

use super::visibility_filter;
use crate::Ruma;

const EVENT_LIMIT: usize = 50;

/// One user's listen on one room's event stream.
///
/// A non-member's listen is a peek, which sees only what a room preview may
/// show.
struct Listen<'a> {
	services: &'a Services,
	sender_user: &'a UserId,
	peeking: bool,
}

/// GET `/_matrix/client/v3/events`
pub(crate) async fn events_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let sender_user = body.sender_user();

	let timeout = body
		.body
		.timeout
		.as_ref()
		.map(Duration::as_millis)
		.map(TryInto::try_into)
		.flat_ok()
		.unwrap_or(services.config.client_sync_timeout_default)
		.max(services.config.client_sync_timeout_min)
		.min(services.config.client_sync_timeout_max);

	let room_id = body.room_id.as_ref();

	let peeking = services
		.state_cache
		.is_joined(sender_user, room_id)
		.await
		.is_false();

	if peeking
		&& services
			.state_accessor
			.is_world_readable(room_id)
			.await
			.is_false()
	{
		return Err!(Request(Forbidden("No room preview available.")));
	}

	// The endpoint listens for new events, so a stream without a token starts now.
	let from = body
		.body
		.from
		.as_deref()
		.map(str::parse)
		.transpose()
		.map_err(|_| err!(Request(InvalidParam("Invalid `from` token."))))?
		.map_async(ok)
		.unwrap_or_else_async(async || {
			services
				.globals
				.wait_pending()
				.await
				.map(PduCount::Normal)
		})
		.await?;

	let listen = Listen {
		services: &services,
		sender_user,
		peeking,
	};

	let stop_at = Instant::now()
		.checked_add(Duration::from_millis(timeout))
		.expect("configuration must limit maximum timeout");

	loop {
		let watchers = services
			.sync
			.watch(sender_user, body.sender_device.as_deref(), once(room_id).stream())
			.await;

		let next_batch = services.globals.wait_pending().await?;

		let window = services
			.timeline
			.pdus(Some(sender_user), room_id, Some(from))
			.ready_filter_map(Result::ok)
			.ready_take_while(|(count, _)| PduCount::Normal(next_batch).ge(count));

		// Any new event answers, hidden or not, so no later wake rescans the window.
		if let Some(response) = window_page(&listen, window, from, next_batch).await {
			return Ok(response);
		}

		if timeout_at(stop_at, watchers).await.is_err() || services.server.is_stopping() {
			return Ok(Response {
				chunk: Default::default(),
				start: from.to_string().into(),
				end: services
					.server
					.is_stopping()
					.is_false()
					.then_some(next_batch)
					.as_ref()
					.map(ToString::to_string),
			});
		}
	}
}

/// The page for a window of new events, or `None` while the window is empty.
///
/// The peek keeps the event it looked at, so the page scans the window once.
async fn window_page<Window>(
	listen: &Listen<'_>,
	window: Window,
	from: PduCount,
	next_batch: u64,
) -> Option<Response>
where
	Window: Stream<Item = PdusIterItem> + Send,
{
	let window = window.peekable();

	pin_mut!(window);
	window.as_mut().peek().await?;

	visible_page(listen, window, from, next_batch)
		.await
		.into()
}

/// The events of a window the user may see, as one page of the stream.
///
/// A full page ends at its last event. A short one scanned the whole window,
/// hidden events included, so it ends at `next_batch`. An empty page starts
/// where the stream did.
async fn visible_page<Window>(
	listen: &Listen<'_>,
	window: Window,
	from: PduCount,
	next_batch: u64,
) -> Response
where
	Window: Stream<Item = PdusIterItem> + Send,
{
	let (first, last, chunk) = window
		.wide_filter_map(|item| listen_filter(listen, item))
		.take(EVENT_LIMIT)
		.ready_fold((None, None, Vec::new()), |(first, _, mut chunk), (count, pdu)| {
			chunk.push(pdu.into_format());
			(first.or(Some(count)), Some(count), chunk)
		})
		.await;

	let start = first.unwrap_or(from).to_string().into();

	let end = last
		.filter(|_| chunk.len().eq(&EVENT_LIMIT))
		.unwrap_or(PduCount::Normal(next_batch))
		.to_string()
		.into();

	Response { start, end, chunk }
}

/// Keeps an event the listener may see.
///
/// A member gets the general history-visibility rule. A peek gets only what a
/// room preview may show: events sent while the room was world-readable, and the
/// event that made it so.
async fn listen_filter(listen: &Listen<'_>, item: PdusIterItem) -> Option<PdusIterItem> {
	if !listen.peeking {
		return visibility_filter(listen.services, item, listen.sender_user).await;
	}

	let (_, pdu) = &item;

	listen
		.services
		.state_accessor
		.is_world_readable_at(pdu)
		.await
		.then_some(item)
}
