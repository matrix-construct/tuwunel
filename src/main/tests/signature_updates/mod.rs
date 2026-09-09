use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	ruma::{
		DeviceId, OwnedUserId, UserId,
		api::client::device::Device,
		serde::{Base64, base64::Standard},
		signatures::{Ed25519KeyPair, KeyPair, to_canonical_json_string_for_signing},
	},
	utils::{ReadyExt, to_canonical_object},
};
use tuwunel_database::{Deserialized, Json, serialize_key};
use tuwunel_service::Services;

pub(super) struct Fixture {
	pub(super) sender: OwnedUserId,
	pub(super) target: OwnedUserId,
	pub(super) target_key: String,
	pub(super) signature: String,
	signer: Ed25519KeyPair,
	signer_key: String,
	key: Value,
}

#[implement(Fixture)]
pub(super) fn new(sender: &UserId, target: &UserId) -> Result<Self> {
	let signer = Ed25519KeyPair::from_der(&Ed25519KeyPair::generate(), "SIGNER".into())
		.expect("generated signer key should parse");

	let root = Ed25519KeyPair::from_der(&Ed25519KeyPair::generate(), "ROOT".into())
		.expect("generated root key should parse");

	let target_key = Base64::<Standard, _>::new(root.public_key()).encode();
	let signer_key = Base64::<Standard, _>::new(signer.public_key()).encode();
	let key = json!({"user_id": target, "usage": ["master"],
		"keys": {format!("ed25519:{target_key}"): target_key}});

	let canonical = to_canonical_json_string_for_signing(&to_canonical_object(&key)?)?;
	let signature = signer.sign(canonical.as_bytes()).base64();

	Ok(Self {
		sender: sender.to_owned(),
		target: target.to_owned(),
		target_key,
		signature,
		signer,
		signer_key,
		key,
	})
}

#[implement(Fixture)]
pub(super) fn with_target(self, other: &Self) -> Result<Self> {
	let canonical = to_canonical_json_string_for_signing(&to_canonical_object(&other.key)?)?;
	let signature = self.signer.sign(canonical.as_bytes()).base64();

	Ok(Self {
		target_key: other.target_key.clone(),
		key: other.key.clone(),
		signature,
		..self
	})
}

#[implement(Fixture)]
pub(super) fn store(&self, services: &Services) -> Result {
	services.db["keyid_key"].put((&self.target, &self.target_key), Json(&self.key));
	services.db["userid_masterkeyid"]
		.insert(&self.target, serialize_key((&self.target, &self.target_key))?);

	self.store_signer(services)
}

#[implement(Fixture)]
pub(super) fn store_signer(&self, services: &Services) -> Result {
	let key = json!({"user_id": self.sender, "usage": ["user_signing"],
		"keys": {format!("ed25519:{}", self.signer_key): self.signer_key}});

	services.db["keyid_key"].put((&self.sender, &self.signer_key), Json(key));
	services.db["userid_usersigningkeyid"]
		.insert(&self.sender, serialize_key((&self.sender, &self.signer_key))?);

	Ok(())
}

#[implement(Fixture)]
pub(super) fn store_device_signer(&self, services: &Services, device: &DeviceId) -> Result {
	self.store(services)?;
	services
		.users
		.put_device_metadata(&self.sender, false, &Device::new(device.to_owned()));

	let key = json!({"user_id": self.sender, "device_id": device,
		"keys": {"ed25519:SIGNER": self.signer_key}});

	services.db["keyid_key"].put((&self.sender, device), Json(key));
	Ok(())
}

#[implement(Fixture)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn upload(&self, services: &Services, signature: &str) -> Result {
	let key_id = self.signing_id();
	let signatures = [(format!("ed25519:{key_id}"), signature.to_owned())].into();

	services
		.users
		.sign_key(&self.target, &self.target_key, signatures, &self.sender)
		.await
}

#[implement(Fixture)]
pub(super) fn signed_key(&self, signature: &str) -> Value {
	with_signature(self.key.clone(), &self.sender, self.signing_id(), signature)
}

#[implement(Fixture)]
fn signing_id(&self) -> &str {
	if self.sender == self.target {
		"SIGNER"
	} else {
		&self.signer_key
	}
}

#[implement(Fixture)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn stored(&self, services: &Services) -> Result<Value> {
	services.db["keyid_key"]
		.qry(&(&self.target, &self.target_key))
		.await?
		.deserialized()
}

#[implement(Fixture)]
pub(super) fn spellings(&self) -> [String; 3] {
	let prefix = self
		.signature
		.get(..85)
		.expect("signature prefix");

	let suffix = match self.signature.as_bytes()[85] {
		| b'A' => 'B',
		| b'Q' => 'R',
		| b'g' => 'h',
		| b'w' => 'x',
		| _ => unreachable!(),
	};

	[
		format!("{}==", self.signature),
		format!("{prefix}{suffix}"),
		self.signature.clone(),
	]
}

fn with_signature(mut key: Value, sender: &UserId, key_id: &str, signature: &str) -> Value {
	key["signatures"] = json!({sender.as_str(): {format!("ed25519:{key_id}"): signature}});
	key
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn assert_changes(
	services: &Services,
	user: &UserId,
	since: u64,
	expected: &[&UserId],
) -> Result {
	let (count, matches) = services
		.users
		.keys_changed(user, since, None)
		.ready_fold((0_usize, true), |(count, matches), user| {
			let next = count
				.checked_add(1)
				.expect("change count fits usize");

			(next, matches && expected.get(count) == Some(&user))
		})
		.await;

	assert_eq!(count, expected.len());
	assert!(matches);

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn register_appservice(services: &Services) -> Result {
	let registration = json!({
		"id": "signature-updates", "url": "http://127.0.0.1:9",
		"as_token": "signature-as", "hs_token": "signature-hs", "sender_localpart": "bridge",
		"namespaces": {"users": [{"exclusive": false, "regex": ".*"}], "aliases": [], "rooms": []},
		"org.matrix.msc3202": true
	});

	let registration = serde_json::from_value(registration)?;

	services
		.appservice
		.register_appservice(registration)
		.await
}
