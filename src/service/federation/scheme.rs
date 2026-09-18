//! Adapters that build the `Input` for ruma's [`AuthScheme`] and
//! [`PathBuilder`] traits from tuwunel's federation context.
//!
//! Federation endpoints span several auth/path-builder combinations
//! (`ServerSignatures` + `VersionHistory`, `ServerSignatures` + `SinglePath`,
//! and a handful of `NoAuthentication`/`NoAccessToken` variants). The
//! generic-associated-type `Input<'a>` of each ruma trait varies per impl, so
//! a single bound on `OutgoingRequest` cannot supply the right value uniformly.
//! [`FedAuth`] and [`FedPath`] each accept tuwunel's federation context and
//! return the appropriate `Input` for the concrete auth scheme or path builder
//! at the call site.

use std::borrow::Cow;

use ruma::{
	OwnedServerName,
	api::{
		SupportedVersions,
		auth_scheme::{AuthScheme, NoAccessToken, NoAuthentication, SendAccessToken},
		federation::authentication::{ServerSignatures, ServerSignaturesInput},
		path_builder::{PathBuilder, SinglePath, VersionHistory},
	},
	signatures::Ed25519KeyPair,
};

/// Builds ruma authentication input from the local federation identity.
///
/// Implementations bridge each concrete [`AuthScheme`] to the common origin,
/// destination, and signing-key context available to the service.
pub trait FedAuth: AuthScheme {
	/// Constructs the authentication input expected by this scheme.
	///
	/// Schemes that do not authenticate ignore some or all supplied context.
	fn input(
		origin: OwnedServerName,
		dest: OwnedServerName,
		keypair: &Ed25519KeyPair,
	) -> <Self as AuthScheme>::Input<'_>;
}

impl FedAuth for NoAuthentication {
	fn input(_: OwnedServerName, _: OwnedServerName, _: &Ed25519KeyPair) {}
}

impl FedAuth for NoAccessToken {
	fn input(_: OwnedServerName, _: OwnedServerName, _: &Ed25519KeyPair) -> SendAccessToken<'_> {
		SendAccessToken::None
	}
}

impl FedAuth for ServerSignatures {
	fn input(
		origin: OwnedServerName,
		dest: OwnedServerName,
		keypair: &Ed25519KeyPair,
	) -> ServerSignaturesInput<'_> {
		ServerSignaturesInput::new(origin, dest, keypair)
	}
}

/// Builds ruma path input from the remote server's supported versions.
///
/// Implementations adapt either a single fixed path or a versioned endpoint
/// history to the same federation request path.
pub trait FedPath: PathBuilder {
	/// Constructs the path-builder input expected by this endpoint.
	///
	/// Single-path endpoints ignore the advertised version history.
	fn input(supported: &SupportedVersions) -> <Self as PathBuilder>::Input<'_>;
}

impl FedPath for SinglePath {
	fn input(_: &SupportedVersions) {}
}

impl FedPath for VersionHistory {
	fn input(supported: &SupportedVersions) -> Cow<'_, SupportedVersions> {
		Cow::Borrowed(supported)
	}
}
