//! Outbound email, compiled out.
//!
//! A build without the `email` feature refuses an SMTP connection URI at startup, so this
//! service is never enabled and every caller takes the path of a server without SMTP.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use tuwunel_core::{Err, Error, Result, err, implement, utils::BoolExt};

use crate::{Args, service::make_name};

/// Stands in for the email service in builds without the `email` feature.
///
/// It holds no transport and reports itself disabled. Startup fails instead when an SMTP
/// connection URI is configured.
#[derive(Clone, Copy)]
pub struct Service;

/// An email address checked only for its shape.
///
/// The address needs a nonempty local part and domain around its final `@`. It stands in for the
/// SMTP library's address type, which this build does not compile.
#[derive(Clone, Debug)]
pub struct Address(String);

impl crate::Service for Service {
	fn build(args: &Args<'_>) -> Result<Arc<Self>> {
		if args.server.config.smtp.connection_uri.is_some() {
			return Err!(Config(
				"smtp.connection_uri",
				"tuwunel was not built with email support (\"email\")"
			));
		}

		Ok(Arc::new(Self))
	}

	fn name(&self) -> &str { make_name(module_path!()) }
}

/// Reports whether outbound email is configured.
///
/// A build without the `email` feature never is, so every caller takes its disabled path.
#[implement(Service)]
#[inline]
#[must_use]
pub fn is_enabled(&self) -> bool { false }

/// Refuses to send an HTML message.
///
/// Callers check [`Self::is_enabled`] first, so a running server never reaches this. It returns
/// the feature-disabled error.
#[implement(Service)]
#[expect(clippy::unused_async)]
pub async fn send_to(&self, _to: &str, _subject: &str, _body_html: String) -> Result {
	Err!(FeatureDisabled("email"))
}

/// Refuses to send an automatic plain-text message.
///
/// Callers check [`Self::is_enabled`] first, so a running server never reaches this. It returns
/// the feature-disabled error.
#[implement(Service)]
#[expect(clippy::unused_async)]
pub async fn send_auto_generated(
	&self,
	_to: &Address,
	_subject: &str,
	_body_text: String,
) -> Result {
	Err!(FeatureDisabled("email"))
}

/// Validates that a string has the shape of an email address.
///
/// The address is not contacted or retained. A value without a nonempty local part and domain
/// around its final `@` maps to `M_INVALID_PARAM`.
#[implement(Service)]
pub fn check_address(&self, to: &str) -> Result { check(to) }

impl TryFrom<String> for Address {
	type Error = Error;

	fn try_from(address: String) -> Result<Self> { check(&address).map(|()| Self(address)) }
}

impl AsRef<str> for Address {
	#[inline]
	fn as_ref(&self) -> &str { &self.0 }
}

fn check(address: &str) -> Result {
	address
		.rsplit_once('@')
		.is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
		.into_option()
		.ok_or_else(|| err!(Request(InvalidParam("Email address is malformed"))))
}
