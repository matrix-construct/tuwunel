use std::collections::BTreeMap;

use futures::StreamExt;
use ruma::{
	api::appservice::{
		Registration,
		thirdparty::{get_location_for_protocol, get_protocol, get_user_for_protocol},
	},
	serde::Raw,
	thirdparty::{Location, Protocol, User},
};
use serde_json::{Value, value::to_raw_value};
use tuwunel_core::{
	implement,
	utils::stream::{IterStream, ReadyExt, WidebandExt},
};

type Protocols = BTreeMap<String, Raw<Protocol>>;

/// Fetches third-party protocol metadata from the registered appservices and
/// keys each response by protocol id. `only` restricts the fan-out to a single
/// protocol.
///
/// Metadata remains opaque JSON so fields unknown to this server survive the
/// forwarding path. When several appservices advertise the same protocol, the
/// first response supplies the metadata. Later responses append array-valued
/// `instances` only when both documents expose that shape. Appservices without
/// a usable destination and failed or undecodable responses contribute nothing
/// rather than failing the client request.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn thirdparty_protocols(&self, only: Option<&str>) -> Protocols {
	let jobs: Vec<(Registration, String)> = self
		.read()
		.await
		.values()
		.filter_map(|info| {
			info.registration
				.protocols
				.as_ref()
				.map(|protocols| (&info.registration, protocols))
		})
		.flat_map(|(registration, protocols)| {
			protocols
				.iter()
				.filter(|protocol| only.is_none_or(|only| only == protocol.as_str()))
				.map(move |protocol| (registration.clone(), protocol.clone()))
		})
		.collect();

	jobs.into_iter()
		.stream()
		.wide_filter_map(async |(registration, protocol)| {
			let request = get_protocol::v1::Request { protocol: protocol.clone() };

			self.send_request(registration, request)
				.await
				.ok()
				.flatten()
				.map(|response| (protocol, Raw::from_json(response.protocol.into_json())))
		})
		.ready_fold(Protocols::new(), |mut protocols, (protocol, metadata)| {
			protocols
				.entry(protocol)
				.and_modify(|existing| merge_instances(existing, &metadata))
				.or_insert(metadata);

			protocols
		})
		.await
}

/// Concatenates the `instances` of `addition` onto `base`, keeping every other
/// field from `base`. Both bodies are opaque JSON, so a malformed side is left
/// alone rather than dropped.
fn merge_instances(base: &mut Raw<Protocol>, addition: &Raw<Protocol>) {
	let (Ok(mut merged), Ok(extra)) =
		(base.deserialize_as::<Value>(), addition.deserialize_as::<Value>())
	else {
		return;
	};

	let (Some(instances), Some(added)) = (
		merged
			.get_mut("instances")
			.and_then(Value::as_array_mut),
		extra.get("instances").and_then(Value::as_array),
	) else {
		return;
	};

	instances.extend(added.iter().cloned());

	if let Ok(raw) = to_raw_value(&merged) {
		*base = Raw::from_json(raw);
	}
}

/// Looks up third-party users on `protocol` via the appservices declaring it,
/// forwarding `fields` to each and concatenating their results.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, fields))]
pub async fn thirdparty_users(
	&self,
	protocol: &str,
	fields: &BTreeMap<String, String>,
) -> Vec<User> {
	self.declaring(protocol)
		.await
		.into_iter()
		.stream()
		.wide_filter_map(async |registration| {
			let request = get_user_for_protocol::v1::Request {
				protocol: protocol.to_owned(),
				fields: forwarded_fields(fields).collect(),
			};

			self.send_request(registration, request)
				.await
				.ok()
				.flatten()
		})
		.map(|response| response.users)
		.concat()
		.await
}

/// Looks up third-party locations on `protocol` via the appservices declaring
/// it, forwarding `fields` to each and concatenating their results.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, fields))]
pub async fn thirdparty_locations(
	&self,
	protocol: &str,
	fields: &BTreeMap<String, String>,
) -> Vec<Location> {
	self.declaring(protocol)
		.await
		.into_iter()
		.stream()
		.wide_filter_map(async |registration| {
			let request = get_location_for_protocol::v1::Request {
				protocol: protocol.to_owned(),
				fields: forwarded_fields(fields).collect(),
			};

			self.send_request(registration, request)
				.await
				.ok()
				.flatten()
		})
		.map(|response| response.locations)
		.concat()
		.await
}

/// Snapshots the registrations declaring `protocol`. Cloning under the read
/// lock lets the fan-out release its guard before the first network await.
#[implement(super::Service)]
async fn declaring(&self, protocol: &str) -> Vec<Registration> {
	self.read()
		.await
		.values()
		.filter(|info| declares(&info.registration, protocol))
		.map(|info| info.registration.clone())
		.collect()
}

/// Drops the client's `access_token` from the forwarded query so a legacy
/// query-param credential never reaches the appservice.
fn forwarded_fields(
	fields: &BTreeMap<String, String>,
) -> impl Iterator<Item = (String, String)> + '_ {
	fields
		.iter()
		.filter(|(name, _)| name.as_str() != "access_token")
		.map(|(name, value)| (name.clone(), value.clone()))
}

fn declares(registration: &Registration, protocol: &str) -> bool {
	registration
		.protocols
		.as_ref()
		.is_some_and(|protocols| {
			protocols
				.iter()
				.any(|declared| declared == protocol)
		})
}
