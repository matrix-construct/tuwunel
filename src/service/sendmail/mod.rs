//! Outbound email delivery.
//!
//! The service builds messages from the configured sender and delivers them through a pooled SMTP
//! transport. It remains disabled when no SMTP connection URI is configured.

#[cfg(test)]
mod tests;

use std::sync::Arc;

pub use lettre::Address;
use lettre::{
	AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
	error::Error as EmailError,
	message::{
		Mailbox, MessageBuilder,
		header::{ContentType, HeaderName, HeaderValue},
	},
};
use tuwunel_core::{Err, Result, err, implement};

/// Delivers outbound email through the configured SMTP transport.
///
/// The service retains a pooled connection and sender mailbox when SMTP is configured. Without a
/// connection URI it remains disabled and rejects delivery attempts.
pub struct Service {
	transport: Option<Transport>,
}

struct Transport {
	smtp: AsyncSmtpTransport<Tokio1Executor>,
	sender: Mailbox,
}

// RFC 3834: automatic responders should not answer mail marked with this field.
const AUTO_SUBMITTED: HeaderName = HeaderName::new_from_ascii_str("Auto-Submitted");

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let smtp = &args.server.config.smtp;
		let transport = smtp
			.connection_uri
			.is_some()
			.then(|| build_transport(smtp))
			.transpose()?;

		Ok(Arc::new(Self { transport }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Reports whether outbound email is configured.
///
/// An enabled service has both a validated sender mailbox and an SMTP transport ready for use.
#[implement(Service)]
#[inline]
#[must_use]
pub fn is_enabled(&self) -> bool { self.transport.is_some() }

/// Sends an HTML message to one recipient from the configured sender.
///
/// Message construction and SMTP delivery occur in one operation. A disabled transport, invalid
/// message, or delivery failure returns an error.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip(self, subject, body_html),
	fields(
		%to,
	),
)]
pub async fn send(&self, to: &Address, subject: &str, body_html: String) -> Result<()> {
	self.deliver(to, subject, |message| {
		message
			.header(ContentType::TEXT_HTML)
			.body(body_html)
	})
	.await
}

/// Sends an automatic plain-text message to one recipient from the configured sender.
///
/// The message is marked `Auto-Submitted: auto-generated` (RFC 3834) so that automatic responders
/// do not answer it. A disabled transport, invalid message, or delivery failure returns an error.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip(self, subject, body_text),
	fields(
		%to,
	),
)]
pub async fn send_auto_generated(
	&self,
	to: &Address,
	subject: &str,
	body_text: String,
) -> Result {
	self.deliver(to, subject, |message| {
		message
			.header(ContentType::TEXT_PLAIN)
			.raw_header(HeaderValue::new(AUTO_SUBMITTED, "auto-generated".to_owned()))
			.body(body_text)
	})
	.await
}

/// Parses a recipient address and sends an HTML message.
///
/// A malformed address maps to `M_INVALID_PARAM`. Valid addresses are delivered through
/// [`Self::send`].
#[implement(Service)]
pub async fn send_to(&self, to: &str, subject: &str, body_html: String) -> Result<()> {
	let to: Address = to
		.parse()
		.map_err(|_| err!(Request(InvalidParam("Email address is malformed"))))?;

	self.send(&to, subject, body_html).await
}

/// Validates that a string parses as an email address.
///
/// The address is not contacted or retained. A malformed value maps to `M_INVALID_PARAM`.
#[implement(Service)]
pub fn check_address(&self, to: &str) -> Result<()> {
	to.parse::<Address>()
		.map(|_| ())
		.map_err(|_| err!(Request(InvalidParam("Email address is malformed"))))
}

#[implement(Service)]
async fn deliver<Build>(&self, to: &Address, subject: &str, build: Build) -> Result
where
	Build: FnOnce(MessageBuilder) -> Result<Message, EmailError>,
{
	let Some(transport) = self.transport.as_ref() else {
		return Err!(Config("smtp", "The email subsystem is not configured"));
	};

	let message = Message::builder()
		.from(transport.sender.clone())
		.to(Mailbox::new(None, to.clone()))
		.subject(subject);

	let message = build(message)
		.map_err(|e| err!(Request(Unknown("Failed to build email message: {e}"))))?;

	transport
		.smtp
		.send(message)
		.await
		.map(|_| ())
		.map_err(|e| err!(Request(Unknown("Failed to send email: {e}"))))
}

fn build_transport(config: &tuwunel_core::config::SmtpConfig) -> Result<Transport> {
	let uri = config.connection_uri.as_deref().ok_or_else(|| {
		err!(Config(
			"smtp.connection_uri",
			"An SMTP connection_uri is required to send email"
		))
	})?;

	let sender = config
		.sender
		.as_deref()
		.ok_or_else(|| err!(Config("smtp.sender", "An SMTP sender mailbox is required")))?
		.parse()
		.map_err(|e| err!(Config("smtp.sender", "Invalid sender mailbox: {e}")))?;

	let smtp = AsyncSmtpTransport::<Tokio1Executor>::from_url(uri)
		.map_err(|e| err!(Config("smtp.connection_uri", "Invalid SMTP connection_uri: {e}")))?
		.build();

	Ok(Transport { smtp, sender })
}
