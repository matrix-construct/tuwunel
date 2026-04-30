use tuwunel_core::{Result, debug, debug_info, debug_warn, implement, trace};

use super::DestString;

#[derive(Clone, Debug)]
pub(super) enum WellKnown {
	/// `m.server` delegation found.
	Delegated(DestString),

	/// Server affirms no delegation (4xx, or 2xx without `m.server`).
	NoDelegation,

	/// Transport, timeout, or 5xx; safe to retry soon.
	Transient,

	/// 2xx with non-JSON / oversized body, or unusual status; suspect peer.
	Adversarial,
}

#[implement(super::Service)]
#[tracing::instrument(
	name = "well-known",
	level = "debug",
	ret(level = "debug"),
	skip(self)
)]
pub(super) async fn request_well_known(&self, dest: &str) -> Result<WellKnown> {
	trace!(%dest, "Requesting well known");
	let response = self
		.services
		.client
		.well_known
		.get(format!("https://{dest}/.well-known/matrix/server"))
		.send()
		.await;

	trace!(?response, "response");
	let response = match response {
		| Ok(r) => r,
		| Err(e) => {
			debug!("transient fetch error: {e:?}");
			return Ok(WellKnown::Transient);
		},
	};

	let status = response.status();
	if !status.is_success() {
		let outcome = match status {
			| _ if status.is_server_error() => WellKnown::Transient,
			| _ if status.is_client_error() => WellKnown::NoDelegation,
			| _ => WellKnown::Adversarial,
		};

		debug!(%status, ?outcome, "non-2xx response");
		return Ok(outcome);
	}

	let text = match response.text().await {
		| Ok(t) => t,
		| Err(e) => {
			debug!("transient body read error: {e:?}");
			return Ok(WellKnown::Transient);
		},
	};

	trace!(?text, "response text");
	if text.len() >= 12288 {
		debug_warn!("oversized body");
		return Ok(WellKnown::Adversarial);
	}

	let body: serde_json::Value = match serde_json::from_str(&text) {
		| Ok(v) => v,
		| Err(e) => {
			debug_warn!("non-JSON body in 2xx response: {e}");
			return Ok(WellKnown::Adversarial);
		},
	};

	let m_server = body
		.get("m.server")
		.and_then(serde_json::Value::as_str)
		.unwrap_or_default();

	if ruma::identifiers_validation::server_name::validate(m_server).is_err() {
		debug!("no usable m.server in body");
		return Ok(WellKnown::NoDelegation);
	}

	debug_info!("{dest:?} found at {m_server:?}");
	Ok(WellKnown::Delegated(m_server.into()))
}
