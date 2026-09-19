use std::{collections::HashMap, future::ready, hash::BuildHasher};

use ruma::{EventId, OwnedEventId};
use serde::Deserialize;
use tuwunel_core::{Result, matrix::PduEvent};

use super::{super::FetchEvent, event_not_found};

impl<F, Fut> FetchEvent for &F
where
	F: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<PduEvent>> + Send,
{
	async fn get<T>(self, event_id: &EventId) -> Result<T>
	where
		T: for<'de> Deserialize<'de> + Send,
	{
		let event = self(event_id.to_owned()).await?;

		project(&event)
	}

	async fn exists(self, event_id: &EventId) -> Result<bool> {
		match self(event_id.to_owned()).await {
			| Ok(_) => Ok(true),
			| Err(error) if error.is_not_found() => Ok(false),
			| Err(error) => Err(error),
		}
	}
}

impl<E, X, EFut, XFut> FetchEvent for (&E, &X)
where
	E: Fn(OwnedEventId) -> EFut + Sync,
	X: Fn(OwnedEventId) -> XFut + Sync,
	EFut: Future<Output = Result<PduEvent>> + Send,
	XFut: Future<Output = Result<bool>> + Send,
{
	fn get<T>(self, event_id: &EventId) -> impl Future<Output = Result<T>> + Send
	where
		T: for<'de> Deserialize<'de> + Send,
	{
		self.0.get(event_id)
	}

	fn exists(self, event_id: &EventId) -> impl Future<Output = Result<bool>> + Send {
		self.1(event_id.to_owned())
	}
}

impl<S: BuildHasher + Sync> FetchEvent for &HashMap<OwnedEventId, PduEvent, S> {
	fn get<T>(self, event_id: &EventId) -> impl Future<Output = Result<T>> + Send
	where
		T: for<'de> Deserialize<'de> + Send,
	{
		ready(
			HashMap::get(self, event_id)
				.ok_or_else(|| event_not_found(event_id))
				.and_then(project),
		)
	}

	fn exists(self, event_id: &EventId) -> impl Future<Output = Result<bool>> + Send {
		ready(Ok(self.contains_key(event_id)))
	}
}

impl<S: BuildHasher + Sync> FetchEvent for &HashMap<OwnedEventId, Vec<u8>, S> {
	fn get<T>(self, event_id: &EventId) -> impl Future<Output = Result<T>> + Send
	where
		T: for<'de> Deserialize<'de> + Send,
	{
		ready(
			HashMap::get(self, event_id)
				.ok_or_else(|| event_not_found(event_id))
				.and_then(|row| serde_json::from_slice(row).map_err(Into::into)),
		)
	}

	fn exists(self, event_id: &EventId) -> impl Future<Output = Result<bool>> + Send {
		ready(Ok(self.contains_key(event_id)))
	}
}

fn project<T: for<'de> Deserialize<'de>>(event: &PduEvent) -> Result<T> {
	let row = serde_json::to_vec(event)?;

	serde_json::from_slice(&row).map_err(Into::into)
}
