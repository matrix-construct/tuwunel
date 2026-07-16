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

pub trait FedAuth: AuthScheme {
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

pub trait FedPath: PathBuilder {
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
