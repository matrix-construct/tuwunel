//! Configures QR sign-in rendezvous sessions.
//!
//! Payload size, lifetime, and session count bound retained handshakes.
//! Authentication policy and per-IP throttling govern discovery and
//! creation.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures QR sign-in rendezvous sessions.
///
/// Payload size, lifetime, and session count bound retained handshakes.
/// Authentication policy and per-IP throttling govern discovery and
/// creation.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rendezvous",
	group = "true"
)]
pub struct Rendezvous {
	/// Enables QR sign-in rendezvous sessions under
	/// [MSC4108](https://github.com/matrix-org/matrix-spec-proposals/pull/4108)
	/// and
	/// [MSC4388](https://github.com/matrix-org/matrix-spec-proposals/pull/4388).
	///
	/// Sessions relay the handshake between two devices before an OAuth
	/// device-authorization grant completes sign-in, requiring the built-in
	/// OIDC server. When disabled, clients hide the feature and the endpoints
	/// return an unrecognized response.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: rendezvous_enabled
	#[serde(default = "true_fn")]
	pub enable: bool,

	/// Limits rendezvous session payloads in bytes, with a default much larger
	/// than normal QR sign-in handshake messages.
	///
	/// reloadable: yes
	/// default: 4096
	/// legacy: rendezvous_session_max_bytes
	#[serde(default = "default_session_max_bytes")]
	pub session_max_bytes: usize,

	/// Sets the rendezvous session lifetime in seconds after its last write.
	///
	/// Each update restarts the window, but the device showing the QR code
	/// times the whole sign-in against the expiry advertised at creation. The
	/// default allows time for interactive account login on the approval page.
	///
	/// reloadable: yes
	/// default: 600
	/// legacy: rendezvous_session_ttl
	#[serde(default = "default_session_ttl")]
	pub session_ttl: u64,

	/// Limits concurrent rendezvous sessions.
	///
	/// Creating beyond the limit evicts the oldest session instead of failing.
	/// Zero retains one session so creation remains available.
	///
	/// reloadable: yes
	/// default: 100
	/// legacy: rendezvous_max_sessions
	#[serde(default = "default_max_sessions")]
	pub max_sessions: usize,

	/// Requires an access token for
	/// [MSC4388](https://github.com/matrix-org/matrix-spec-proposals/pull/4388)
	/// discovery and session creation.
	///
	/// When disabled, clients can discover and create these sessions without a
	/// token. The
	/// [MSC4108](https://github.com/matrix-org/matrix-spec-proposals/pull/4108)
	/// endpoint stays open in either mode.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: rendezvous_authenticated_only
	#[serde(default = "true_fn")]
	pub authenticated_only: bool,

	/// Sets the per-client-IP refill rate for
	/// [MSC4388](https://github.com/matrix-org/matrix-spec-proposals/pull/4388)
	/// rendezvous requests.
	///
	/// Zero is treated as one request per second.
	///
	/// reloadable: yes
	/// default: 10
	/// legacy: rendezvous_rc_per_second
	#[serde(default = "default_rc_per_second")]
	pub rc_per_second: u32,

	/// Sets the token-bucket depth for the rendezvous request throttle under
	/// [MSC4388](https://github.com/matrix-org/matrix-spec-proposals/pull/4388).
	///
	/// This is the requests one client IP can make in a burst before
	/// `rendezvous.rc_per_second` governs replenishment. Zero is treated as
	/// one.
	///
	/// reloadable: yes
	/// default: 20
	/// legacy: rendezvous_rc_burst_count
	#[serde(default = "default_rc_burst_count")]
	pub rc_burst_count: u32,
}

impl Default for Rendezvous {
	fn default() -> Self {
		Self {
			enable: true_fn(),
			session_max_bytes: default_session_max_bytes(),
			session_ttl: default_session_ttl(),
			max_sessions: default_max_sessions(),
			authenticated_only: true_fn(),
			rc_per_second: default_rc_per_second(),
			rc_burst_count: default_rc_burst_count(),
		}
	}
}

fn default_session_max_bytes() -> usize { 4096 }

fn default_session_ttl() -> u64 { 600 }

fn default_max_sessions() -> usize { 100 }

fn default_rc_per_second() -> u32 { 10 }

fn default_rc_burst_count() -> u32 { 20 }
