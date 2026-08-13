use std::collections::BTreeMap;

use ruma::api::{
	client::{
		discovery::discover_support::Contact,
		rtc::RtcTransport,
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

/// Build the configured RTC transports as `RtcTransport` values, the typed
/// form shared between `.well-known/matrix/client.rtc_foci` and the
/// `/rtc/transports` endpoint.
#[implement(super::WellKnownConfig)]
pub fn get_transports(&self) -> Result<Vec<RtcTransport>> {
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

	let livekit_url = self
		.livekit_url
		.iter()
		.cloned()
		.map(|url| Ok(RtcTransport::livekit(url)));

	custom
		.chain(livekit_url)
		.collect::<Result<Vec<_>>>()
		.inspect_err(inspect_log)
}
