use std::{iter::once, slice::from_ref};

use futures::StreamExt;
use http::StatusCode;
use ruma::{OwnedServerName, ServerName, api::error::ErrorBody};
use serde_json::Value;
use tuwunel_core::{
	Err, Error, Result,
	config::Figment,
	err,
	matrix::{PduCount, PduId},
	utils::time::now_secs,
};

use super::{Rooms, Slice, Split};
use crate::{
	sending::{
		Destination, EduBuf, SendingEvent, Service,
		data::{Park, QueueItem},
		sender::{
			NewEvents, SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue,
			dispatch::Completion,
			select::Selection,
			tests::{delivered, enqueue},
		},
	},
	test_utils::{Fixture, fixture},
};

#[tokio::test]
async fn only_repeated_content_failures_split() -> Result {
	let Some(fixture) = sender_fixture().await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server: OwnedServerName = "split.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let edu = active(sending, &dest, SendingEvent::Edu(EduBuf::from_slice(b"{}")));
	let forbidden = rejection(&server, StatusCode::FORBIDDEN);
	let throttled = rejection(&server, StatusCode::TOO_MANY_REQUESTS);
	let edus_only = sending
		.split_failure(&server, &forbidden, None, 4)
		.await;

	assert_eq!(edus_only, (None, 4));

	let pdu = active(sending, &dest, room_pdu(1, 1));

	for (error, tries) in [(&forbidden, 3), (&throttled, 4)] {
		let split = sending
			.split_failure(&server, error, None, tries)
			.await;

		assert_eq!(split, (None, tries));
	}

	let split = sending
		.split_failure(&server, &forbidden, None, 4)
		.await;

	assert_eq!(split, (Some(Split::new(Rooms::from_slice(&[1]))), 0));

	let queued: Vec<_> = sending.db.queued_requests(&dest).collect().await;
	let active: Vec<_> = sending
		.db
		.active_requests_for(&dest)
		.collect()
		.await;

	assert_eq!((active, queued), (vec![edu], vec![pdu]));

	Ok(())
}

#[tokio::test]
async fn rooms_go_out_apart_until_one_is_parked() -> Result {
	let Some(fixture) = sender_fixture().await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server: OwnedServerName = "split.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let first = active(sending, &dest, room_pdu(1, 1));
	let second = active(sending, &dest, room_pdu(2, 2));
	let edu = active(sending, &dest, SendingEvent::Edu(EduBuf::from_slice(b"{}")));
	let mut futures = SendingFutures::new(); // handle_response out-param
	let mut statuses: TransactionStatuses =
		[(dest.clone(), TransactionStatus::Running { tries: 3 })].into();

	let mut wakes = WakeQueue::new(); // handle_response out-param

	sending
		.handle_response(failed(&server, None), &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(matches!(
		statuses.get(&dest),
		Some(TransactionStatus::Splitting { tries: 0, .. })
	));

	assert_eq!(wakes.len(), 1);

	let (items, split) = selected(sending, &dest, &mut statuses).await?;

	assert_eq!(items.len(), 2);
	assert!(items.contains(&first) && items.contains(&edu));

	let completion = advanced(&dest, (items, split));

	sending
		.handle_response(completion, &mut futures, &mut statuses, &mut wakes)
		.await;

	let active: Vec<_> = sending
		.db
		.active_requests_for(&dest)
		.collect()
		.await;

	assert_eq!((futures.len(), active), (1, vec![second.clone()]));

	let split = Split::new([1, 2].into()).delivered();

	sending
		.handle_response(failed(&server, Some(split)), &mut futures, &mut statuses, &mut wakes)
		.await;

	let queued: Vec<_> = sending.db.queued_requests(&dest).collect().await;
	let parks: Vec<_> = sending.db.parks(&server).collect().await;

	assert_eq!(queued, from_ref(&second));
	assert!(matches!(parks[..], [Park { room: 2, count: 1, .. }]));

	let selection = sending
		.select_events(&dest, NewEvents::new(), &mut statuses)
		.await?;

	assert!(matches!(selection, Selection::Parked { .. }));

	Ok(())
}

#[tokio::test]
async fn an_expired_park_retries_its_room_alone() -> Result {
	let Some(fixture) = sender_fixture().await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server: OwnedServerName = "split.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let parked = enqueue(sending, &dest, room_pdu(1, 1));
	let healthy = enqueue(sending, &dest, room_pdu(2, 2));
	let mut futures = SendingFutures::new(); // handle_response out-param
	let mut statuses = TransactionStatuses::new(); // handle_response out-param
	let mut wakes = WakeQueue::new(); // handle_response out-param

	park(sending, &server, 1, now_secs().saturating_add(3600));

	let selection = sending
		.select_events(&dest, [parked.clone()].into(), &mut statuses)
		.await?;

	assert_eq!(selection, Selection::Events(vec![healthy.clone()]));

	sending
		.handle_response(
			delivered(&dest, vec![healthy.0]),
			&mut futures,
			&mut statuses,
			&mut wakes,
		)
		.await;

	assert!(statuses.is_empty());
	assert_eq!(wakes.len(), 1);

	park(sending, &server, 0, 0);
	park(sending, &server, 1, 0);

	let (items, split) = selected(sending, &dest, &mut statuses).await?;

	assert_eq!(items, from_ref(&parked));

	sending
		.handle_response(failed(&server, Some(split)), &mut futures, &mut statuses, &mut wakes)
		.await;

	let parks: Vec<_> = sending.db.parks(&server).collect().await;

	assert!(matches!(
		parks[..],
		[Park { room: 1, count: 2, until }] if until > now_secs().saturating_add(3600)
	));

	park(sending, &server, 1, 0);

	let (items, split) = selected(sending, &dest, &mut statuses).await?;

	sending
		.handle_response(advanced(&dest, (items, split)), &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(sending.db.parks(&server).count().await, 0);

	Ok(())
}

#[tokio::test]
async fn a_lone_rejected_room_brings_in_a_control() -> Result {
	let Some(fixture) = sender_fixture().await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server: OwnedServerName = "split.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let lone = active(sending, &dest, room_pdu(1, 1));
	let control = enqueue(sending, &dest, room_pdu(3, 2));
	let forbidden = rejection(&server, StatusCode::FORBIDDEN);
	let (Some(split), 0) = sending
		.split_failure(&server, &forbidden, None, 4)
		.await
	else {
		return Err!("four rejections split the room out");
	};

	let (items, split) = slice(sending, &dest, split).await?;

	assert_eq!(items, from_ref(&lone));

	let (Some(split), 1) = sending
		.split_failure(&server, &forbidden, Some(split), 1)
		.await
	else {
		return Err!("a failure before any delivery waits out its backoff");
	};

	assert_eq!(split.0.rooms, Rooms::from([3, 1]));

	let (items, split) = slice(sending, &dest, split).await?;

	assert_eq!(items, from_ref(&control));

	sending.db.delete_active_requests(&[control.0]);

	let (items, split) = slice(sending, &dest, split.delivered()).await?;

	assert_eq!(items, [lone]);

	let (split, tries) = sending
		.split_failure(&server, &forbidden, Some(split), 1)
		.await;

	assert!(split.is_some_and(|split| split.0.rooms.is_empty()));
	assert_eq!(tries, 0);

	Ok(())
}

#[tokio::test]
async fn a_transaction_that_sends_nothing_ends_its_split() -> Result {
	let Some(fixture) = sender_fixture().await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Federation("split.example".try_into()?);
	let unloadable = enqueue(sending, &dest, room_pdu(1, 1));
	let Completion { result: Ok(_), split: None, .. } = sending
		.send_events(dest, vec![unloadable], Some(Split::new([1, 2].into())))
		.await
	else {
		return Err!("a transaction sending nothing proves no delivery");
	};

	Ok(())
}

async fn sender_fixture() -> Result<Option<Fixture>> {
	fixture(Figment::new().merge(("sender_workers", 1))).await
}

fn active(sending: &Service, dest: &Destination, event: SendingEvent) -> QueueItem {
	let item = enqueue(sending, dest, event);

	sending.db.mark_as_active(once(&item));
	item
}

fn rejection(server: &ServerName, status: StatusCode) -> Error {
	Error::Federation(server.to_owned(), ErrorBody::Json(Value::Null).into_error(status))
}

fn room_pdu(room: u64, count: u64) -> SendingEvent {
	SendingEvent::Pdu(
		PduId {
			shortroomid: room,
			count: PduCount::Normal(count),
		}
		.into(),
	)
}

fn failed(server: &ServerName, split: Option<Split>) -> Completion {
	let dest = Destination::Federation(server.to_owned());
	let result = Err((dest, rejection(server, StatusCode::FORBIDDEN)));

	Completion { result, keys: Vec::new(), split }
}

async fn selected(
	sending: &Service,
	dest: &Destination,
	statuses: &mut TransactionStatuses,
) -> Result<Slice> {
	match sending
		.select_events(dest, NewEvents::new(), statuses)
		.await?
	{
		| Selection::Slice(items, split) => Ok((items, split)),
		| selection => Err!("expected one room's slice, selected {selection:?}"),
	}
}

fn advanced(dest: &Destination, (items, split): Slice) -> Completion {
	let keys = items.into_iter().map(|(key, _)| key).collect();

	Completion {
		split: Some(split),
		..delivered(dest, keys)
	}
}

fn park(sending: &Service, server: &ServerName, room: u64, until: u64) {
	sending
		.db
		.demote(&[], Some((server, Park { room, until, count: 1 })));
}

async fn slice(sending: &Service, dest: &Destination, split: Split) -> Result<Slice> {
	sending
		.slice(dest, split)
		.await
		.ok_or_else(|| err!("the split has a room left"))
}
