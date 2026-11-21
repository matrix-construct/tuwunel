//! Converts discovery configuration into Matrix API values.
//!
//! Helpers build support contacts, policies, registration terms, and MatrixRTC
//! transports from config. Endpoint handlers share these conversions.

use std::collections::BTreeMap;

#[expect(deprecated)]
use ruma::api::{
	client::{
		discovery::discover_support::Contact,
		rtc::{LegacyLiveKitRtcTransport, RtcTransport},
		uiaa::{LoginTermsParams, PolicyDefinition, PolicyTranslation},
	},
	identity_service::tos::get_terms_of_service::v2::{LocalizedPolicy, Policies},
};
use tuwunel_macros::implement;

use crate::{Result, err, error::inspect_log};

/// Builds the support contacts advertised through well-known discovery.
///
/// Named contact entries are converted first. Legacy single-contact fields
/// append one additional contact when a support role is configured.
#[implement(super::WellKnownConfig)]
pub fn get_contacts(&self) -> Vec<Contact> {
	let single_contact = self.support_role.clone().map(|role| Contact {
		role,
		email_address: self.support_email.clone(),
		matrix_id: self.support_mxid.clone(),
		pgp_key: self.support_pgp_key.clone(),
	});

	let contacts = self
		.support_contact
		.clone()
		.into_values()
		.map(Into::into);

	contacts.chain(single_contact).collect()
}

/// Builds the localized support policies advertised through discovery.
///
/// Outer map keys remain policy identifiers. Each configured language entry is
/// converted into the corresponding Matrix policy representation.
#[implement(super::WellKnownConfig)]
#[must_use]
pub fn get_policies(&self) -> BTreeMap<String, Policies> {
	self.support_policy
		.iter()
		.map(|(id, policy)| {
			let localized = policy
				.policy_translation
				.iter()
				.map(|(language, translation)| {
					(language.clone(), LocalizedPolicy::from(translation.clone()))
				})
				.collect();

			(id.clone(), Policies {
				version: policy.version.clone(),
				localized,
			})
		})
		.collect()
}

/// Builds registration terms parameters from configured policy documents.
///
/// Policy identifiers and language keys are preserved in the Matrix UIA shape.
/// An empty policy map returns `None` and does not require a terms stage.
#[implement(super::Config)]
#[must_use]
pub fn login_terms_params(&self) -> Option<LoginTermsParams> {
	let policies: BTreeMap<_, _> = self
		.registration_terms
		.iter()
		.map(|(id, policy)| {
			let translations = policy
				.translations
				.iter()
				.map(|(language, translation)| {
					let translation = PolicyTranslation::new(
						translation.name.clone(),
						translation.url.to_string(),
					);

					(language.clone(), translation)
				})
				.collect();

			(id.clone(), PolicyDefinition::new(policy.version.clone(), translations))
		})
		.collect();

	(!policies.is_empty()).then(|| LoginTermsParams::new(policies))
}

/// Builds the MatrixRTC transports advertised to clients.
///
/// The operator's `rtc_transports` entries come first, in their order, then
/// the native LiveKit transport when `[global.livekit]` is set, then the legacy
/// `livekit_url` one; MSC4143 reads the list as the server's order of
/// preference. `.well-known/matrix/client`, `/rtc/transports` and the startup
/// check all read this one list.
#[implement(super::Config)]
#[inline]
pub fn rtc_transports(&self) -> Result<Vec<RtcTransport>> {
	self.well_known
		.get_transports(self.livekit.transport())
}

#[implement(super::WellKnownConfig)]
fn get_transports(&self, native: Option<RtcTransport>) -> Result<Vec<RtcTransport>> {
	let custom = self.rtc_transports.iter().map(|item| {
		let mut data = item
			.as_object()
			.cloned()
			.ok_or_else(|| err!("`rtc_transport` is not a valid object"))?;

		let transport_type = data
			.remove("type")
			.and_then(|v| v.as_str().map(str::to_owned))
			.ok_or_else(|| err!("`type` is not a valid string"))?;

		RtcTransport::new(&transport_type, data).map_err(|e| {
			err!(Config("global.well_known.rtc_transports", "Malformed value(s): {e:?}"))
		})
	});

	#[expect(deprecated)]
	let livekit_url = self.livekit_url.iter().cloned().map(|url| {
		Ok(RtcTransport::LegacyLiveKit(LegacyLiveKitRtcTransport::with_service_url(url)))
	});

	custom
		.chain(native.map(Ok))
		.chain(livekit_url)
		.collect::<Result<Vec<_>>>()
		.inspect_err(inspect_log)
}

#[cfg(test)]
mod tests {
	use serde_json::{json, to_value};

	use crate::{Result, config::WellKnownConfig};

	#[test]
	fn livekit_transport_keeps_service_url_wire() -> Result {
		let url = "https://livekit.example.org";
		let config = WellKnownConfig {
			livekit_url: Some(url.into()),
			..Default::default()
		};

		let transports = config.get_transports(None)?;

		assert_eq!(
			to_value(&transports[0])?,
			json!({
				"type": "livekit",
				"livekit_service_url": url,
			})
		);
		Ok(())
	}
}
