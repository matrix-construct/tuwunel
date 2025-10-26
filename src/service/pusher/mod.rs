mod append;

use std::{fmt::Debug, mem, sync::Arc};

use bytes::BytesMut;
use futures::{Stream, StreamExt};
use ipaddress::IPAddress;
use ruma::{
	DeviceId, OwnedDeviceId, RoomId, UInt, UserId,
	api::{
		IncomingResponse, MatrixVersion, OutgoingRequest, SendAccessToken, SupportedVersions,
		client::push::{Pusher, PusherKind, set_pusher},
		push_gateway::send_event_notification::{
			self,
			v1::{Device, Notification, NotificationCounts, NotificationPriority},
		},
	},
	events::{AnySyncTimelineEvent, TimelineEventType, room::power_levels::RoomPowerLevels},
	push::{
		Action, PushConditionPowerLevelsCtx, PushConditionRoomCtx, PushFormat, Ruleset, Tweak,
	},
	serde::Raw,
	uint,
};
use tuwunel_core::{
	Err, Result, debug_warn, err,
	matrix::Event,
	trace,
	utils::{
		stream::{BroadbandExt, TryIgnore},
		string_from_bytes,
	},
	warn,
};
use tuwunel_database::{Database, Deserialized, Ignore, Interfix, Json, Map};

pub struct Service {
	db: Data,
	services: Arc<crate::services::OnceServices>,
}

struct Data {
	senderkey_pusher: Arc<Map>,
	pushkey_deviceid: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
	db: Arc<Database>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				senderkey_pusher: args.db["senderkey_pusher"].clone(),
				pushkey_deviceid: args.db["pushkey_deviceid"].clone(),
				userroomid_highlightcount: args.db["userroomid_highlightcount"].clone(),
				userroomid_notificationcount: args.db["userroomid_notificationcount"].clone(),
				db: args.db.clone(),
			},
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub async fn set_pusher(
		&self,
		sender: &UserId,
		sender_device: &DeviceId,
		pusher: &set_pusher::v3::PusherAction,
	) -> Result {
		match pusher {
			| set_pusher::v3::PusherAction::Post(data) => {
				let pushkey = data.pusher.ids.pushkey.as_str();

				if pushkey.len() > 512 {
					return Err!(Request(InvalidParam(
						"Push key length cannot be greater than 512 bytes."
					)));
				}

				if data.pusher.ids.app_id.as_str().len() > 64 {
					return Err!(Request(InvalidParam(
						"App ID length cannot be greater than 64 bytes."
					)));
				}

				// add some validation to the pusher URL
				let pusher_kind = &data.pusher.kind;
				if let PusherKind::Http(http) = pusher_kind {
					let url = &http.url;
					let url = url::Url::parse(&http.url).map_err(|e| {
						err!(Request(InvalidParam(
							warn!(%url, "HTTP pusher URL is not a valid URL: {e}")
						)))
					})?;

					if ["http", "https"]
						.iter()
						.all(|&scheme| scheme != url.scheme().to_lowercase())
					{
						return Err!(Request(InvalidParam(
							warn!(%url, "HTTP pusher URL is not a valid HTTP/HTTPS URL")
						)));
					}

					if let Ok(ip) =
						IPAddress::parse(url.host_str().expect("URL previously validated"))
					{
						if !self.services.client.valid_cidr_range(&ip) {
							return Err!(Request(InvalidParam(
								warn!(%url, "HTTP pusher URL is a forbidden remote address")
							)));
						}
					}
				}

				let pushkey = data.pusher.ids.pushkey.as_str();
				let key = (sender, pushkey);
				self.db.senderkey_pusher.put(key, Json(pusher));
				self.db
					.pushkey_deviceid
					.insert(pushkey, sender_device);
			},
			| set_pusher::v3::PusherAction::Delete(ids) => {
				self.delete_pusher(sender, ids.pushkey.as_str())
					.await;
			},
		}

		Ok(())
	}

	pub async fn delete_pusher(&self, sender: &UserId, pushkey: &str) {
		let key = (sender, pushkey);
		self.db.senderkey_pusher.del(key);
		self.db.pushkey_deviceid.remove(pushkey);

		self.services
			.sending
			.cleanup_events(None, Some(sender), Some(pushkey))
			.await
			.ok();
	}

	pub async fn get_device_pushkeys(
		&self,
		sender: &UserId,
		device_id: &DeviceId,
	) -> Vec<String> {
		self.get_pushkeys(sender)
			.map(ToOwned::to_owned)
			.broad_filter_map(async |pushkey| {
				self.get_pusher_device(&pushkey)
					.await
					.ok()
					.filter(|pusher_device| pusher_device == device_id)
					.is_some()
					.then_some(pushkey)
			})
			.collect()
			.await
	}

	pub async fn get_pusher_device(&self, pushkey: &str) -> Result<OwnedDeviceId> {
		self.db
			.pushkey_deviceid
			.get(pushkey)
			.await
			.deserialized()
	}

	pub async fn get_pusher(&self, sender: &UserId, pushkey: &str) -> Result<Pusher> {
		let senderkey = (sender, pushkey);
		self.db
			.senderkey_pusher
			.qry(&senderkey)
			.await
			.deserialized()
	}

	pub async fn get_pushers(&self, sender: &UserId) -> Vec<Pusher> {
		let prefix = (sender, Interfix);
		self.db
			.senderkey_pusher
			.stream_prefix(&prefix)
			.ignore_err()
			.map(|(_, pusher): (Ignore, Pusher)| pusher)
			.collect()
			.await
	}

	pub fn get_pushkeys<'a>(
		&'a self,
		sender: &'a UserId,
	) -> impl Stream<Item = &str> + Send + 'a {
		let prefix = (sender, Interfix);
		self.db
			.senderkey_pusher
			.keys_prefix(&prefix)
			.ignore_err()
			.map(|(_, pushkey): (Ignore, &str)| pushkey)
	}

	#[tracing::instrument(skip(self, dest, request))]
	pub async fn send_request<T>(&self, dest: &str, request: T) -> Result<T::IncomingResponse>
	where
		T: OutgoingRequest + Debug + Send,
	{
		const VERSIONS: [MatrixVersion; 1] = [MatrixVersion::V1_0];
		let supported = SupportedVersions {
			versions: VERSIONS.into(),
			features: Default::default(),
		};

		let dest = dest.replace(&self.services.config.notification_push_path, "");
		trace!("Push gateway destination: {dest}");

		let http_request = request
			.try_into_http_request::<BytesMut>(&dest, SendAccessToken::IfRequired(""), &supported)
			.map_err(|e| {
				err!(BadServerResponse(warn!(
					"Failed to find destination {dest} for push gateway: {e}"
				)))
			})?
			.map(BytesMut::freeze);

		let reqwest_request = reqwest::Request::try_from(http_request)?;

		if let Some(url_host) = reqwest_request.url().host_str() {
			trace!("Checking request URL for IP");
			if let Ok(ip) = IPAddress::parse(url_host) {
				if !self.services.client.valid_cidr_range(&ip) {
					return Err!(BadServerResponse("Not allowed to send requests to this IP"));
				}
			}
		}

		let response = self
			.services
			.client
			.pusher
			.execute(reqwest_request)
			.await;

		match response {
			| Ok(mut response) => {
				// reqwest::Response -> http::Response conversion

				trace!("Checking response destination's IP");
				if let Some(remote_addr) = response.remote_addr() {
					if let Ok(ip) = IPAddress::parse(remote_addr.ip().to_string()) {
						if !self.services.client.valid_cidr_range(&ip) {
							return Err!(BadServerResponse(
								"Not allowed to send requests to this IP"
							));
						}
					}
				}

				let status = response.status();
				let mut http_response_builder = http::Response::builder()
					.status(status)
					.version(response.version());
				mem::swap(
					response.headers_mut(),
					http_response_builder
						.headers_mut()
						.expect("http::response::Builder is usable"),
				);

				let body = response.bytes().await?; // TODO: handle timeout

				if !status.is_success() {
					debug_warn!("Push gateway response body: {:?}", string_from_bytes(&body));
					return Err!(BadServerResponse(warn!(
						"Push gateway {dest} returned unsuccessful HTTP response: {status}"
					)));
				}

				let response = T::IncomingResponse::try_from_http_response(
					http_response_builder
						.body(body)
						.expect("reqwest body is valid http body"),
				);
				response.map_err(|e| {
					err!(BadServerResponse(warn!(
						"Push gateway {dest} returned invalid response: {e}"
					)))
				})
			},
			| Err(e) => {
				warn!("Could not send request to pusher {dest}: {e}");
				Err(e.into())
			},
		}
	}

	#[tracing::instrument(skip(self, user, unread, pusher, ruleset, event))]
	pub async fn send_push_notice<E>(
		&self,
		user: &UserId,
		unread: UInt,
		pusher: &Pusher,
		ruleset: Ruleset,
		event: &E,
	) -> Result
	where
		E: Event,
	{
		let mut notify = None;
		let mut tweaks = Vec::new();

		let power_levels: RoomPowerLevels = self
			.services
			.state_accessor
			.get_power_levels(event.room_id())
			.await?;

		let serialized = event.to_format();
		for action in self
			.get_actions(user, &ruleset, &power_levels, &serialized, event.room_id())
			.await
		{
			let n = match action {
				| Action::Notify => true,
				| Action::SetTweak(tweak) => {
					tweaks.push(tweak.clone());
					continue;
				},
				| _ => false,
			};

			if notify.is_some() {
				return Err!(Database(
					r#"Malformed pushrule contains more than one of these actions: ["dont_notify", "notify", "coalesce"]"#
				));
			}

			notify = Some(n);
		}

		if notify == Some(true) {
			self.send_notice(unread, pusher, tweaks, event)
				.await?;
		}
		// Else the event triggered no actions

		Ok(())
	}

	#[tracing::instrument(skip(self, user, ruleset, pdu), level = "debug")]
	pub async fn get_actions<'a>(
		&self,
		user: &UserId,
		ruleset: &'a Ruleset,
		power_levels: &RoomPowerLevels,
		pdu: &Raw<AnySyncTimelineEvent>,
		room_id: &RoomId,
	) -> &'a [Action] {
		let power_levels = PushConditionPowerLevelsCtx {
			users: power_levels.users.clone(),
			users_default: power_levels.users_default,
			notifications: power_levels.notifications.clone(),
			rules: power_levels.rules.clone(),
		};

		let room_joined_count = self
			.services
			.state_cache
			.room_joined_count(room_id)
			.await
			.unwrap_or(1)
			.try_into()
			.unwrap_or_else(|_| uint!(0));

		let user_display_name = self
			.services
			.users
			.displayname(user)
			.await
			.unwrap_or_else(|_| user.localpart().to_owned());

		let ctx = PushConditionRoomCtx {
			room_id: room_id.to_owned(),
			member_count: room_joined_count,
			user_id: user.to_owned(),
			user_display_name,
			power_levels: Some(power_levels),
		};

		ruleset.get_actions(pdu, &ctx).await
	}

	#[tracing::instrument(skip(self, unread, pusher, tweaks, event))]
	async fn send_notice<Pdu: Event>(
		&self,
		unread: UInt,
		pusher: &Pusher,
		tweaks: Vec<Tweak>,
		event: &Pdu,
	) -> Result {
		// TODO: email
		match &pusher.kind {
			| PusherKind::Http(http) => {
				let url = &http.url;
				let url = url::Url::parse(&http.url).map_err(|e| {
					err!(Request(InvalidParam(
						warn!(%url, "HTTP pusher URL is not a valid URL: {e}")
					)))
				})?;

				if ["http", "https"]
					.iter()
					.all(|&scheme| scheme != url.scheme().to_lowercase())
				{
					return Err!(Request(InvalidParam(
						warn!(%url, "HTTP pusher URL is not a valid HTTP/HTTPS URL")
					)));
				}

				if let Ok(ip) =
					IPAddress::parse(url.host_str().expect("URL previously validated"))
				{
					if !self.services.client.valid_cidr_range(&ip) {
						return Err!(Request(InvalidParam(
							warn!(%url, "HTTP pusher URL is a forbidden remote address")
						)));
					}
				}

				// TODO (timo): can pusher/devices have conflicting formats
				let event_id_only = http.format == Some(PushFormat::EventIdOnly);

				let mut device =
					Device::new(pusher.ids.app_id.clone(), pusher.ids.pushkey.clone());
				device.data.data.clone_from(&http.data);
				device.data.format.clone_from(&http.format);

				// Tweaks are only added if the format is NOT event_id_only
				if !event_id_only {
					device.tweaks.clone_from(&tweaks);
				}

				let d = vec![device];
				let mut notifi = Notification::new(d);

				notifi.event_id = Some(event.event_id().to_owned());
				notifi.room_id = Some(event.room_id().to_owned());
				if http
					.data
					.get("org.matrix.msc4076.disable_badge_count")
					.is_none() && http.data.get("disable_badge_count").is_none()
				{
					notifi.counts = NotificationCounts::new(unread, uint!(0));
				} else {
					// counts will not be serialised if it's the default (0, 0)
					// skip_serializing_if = "NotificationCounts::is_default"
					notifi.counts = NotificationCounts::default();
				}

				if !event_id_only {
					if *event.kind() == TimelineEventType::RoomEncrypted
						|| tweaks
							.iter()
							.any(|t| matches!(t, Tweak::Highlight(true) | Tweak::Sound(_)))
					{
						notifi.prio = NotificationPriority::High;
					} else {
						notifi.prio = NotificationPriority::Low;
					}
					notifi.sender = Some(event.sender().to_owned());
					notifi.event_type = Some(event.kind().to_owned());
					notifi.content = serde_json::value::to_raw_value(event.content()).ok();

					if *event.kind() == TimelineEventType::RoomMember {
						notifi.user_is_target =
							event.state_key() == Some(event.sender().as_str());
					}

					notifi.sender_display_name = self
						.services
						.users
						.displayname(event.sender())
						.await
						.ok();

					notifi.room_name = self
						.services
						.state_accessor
						.get_name(event.room_id())
						.await
						.ok();

					notifi.room_alias = self
						.services
						.state_accessor
						.get_canonical_alias(event.room_id())
						.await
						.ok();
				}

				self.send_request(&http.url, send_event_notification::v1::Request::new(notifi))
					.await?;

				Ok(())
			},
			// TODO: Handle email
			//PusherKind::Email(_) => Ok(()),
			| _ => Ok(()),
		}
	}
}
