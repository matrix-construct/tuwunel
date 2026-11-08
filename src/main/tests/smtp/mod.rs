//! A one-message SMTP sink for tests that send mail.
//!
//! It stands apart from the client harness so that only a binary that sends
//! mail compiles it.

use std::net::TcpListener;

use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
	net::{TcpListener as TokioTcpListener, tcp::OwnedReadHalf},
};
use tuwunel_core::{Err, Result, err};

type ClientLines = Lines<BufReader<OwnedReadHalf>>;

/// Accepts one SMTP connection and returns the first message delivered on it.
///
/// Bind the listener before booting the server, so nothing else can claim the
/// port in between. The message comes back as sent, headers and encoded body,
/// with the dot stuffing undone. Every command but `DATA` and `QUIT` is
/// accepted; a `QUIT` or a disconnect before the message ends returns an
/// error.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn capture_email(listener: TcpListener) -> Result<String> {
	listener.set_nonblocking(true)?;

	let listener = TokioTcpListener::from_std(listener)?;
	let (stream, _) = listener.accept().await?;
	let (reader, mut writer) = stream.into_split(); // write_all takes &mut self
	let mut lines = BufReader::new(reader).lines(); // next_line takes &mut self

	writer
		.write_all(b"220 localhost ESMTP\r\n")
		.await?;

	loop {
		let line = lines
			.next_line()
			.await?
			.ok_or_else(|| err!("SMTP client closed before sending a message"))?;

		let reply: &[u8] = match line.as_str() {
			| "DATA" => break,
			| _ if line.starts_with("EHLO ") || line.starts_with("HELO ") => b"250 localhost\r\n",
			| "QUIT" => {
				writer.write_all(b"221 Bye\r\n").await?;

				return Err!("SMTP client quit before sending a message");
			},
			| _ => b"250 OK\r\n",
		};

		writer.write_all(reply).await?;
	}

	writer
		.write_all(b"354 End data with <CRLF>.<CRLF>\r\n")
		.await?;

	let message = message_data(&mut lines).await?;

	writer
		.write_all(b"250 Message accepted\r\n")
		.await?;

	Ok(message)
}

/// Reads message data up to the line holding a lone dot.
///
/// A leading dot the client added to a line starting with one is removed, as
/// RFC 5321 section 4.5.2 asks.
#[tracing::instrument(level = "debug", skip_all)]
async fn message_data(lines: &mut ClientLines) -> Result<String> {
	let mut message = String::with_capacity(1024);

	while let Some(line) = lines.next_line().await? {
		if line == "." {
			return Ok(message);
		}

		message.push_str(line.strip_prefix('.').unwrap_or(&line));
		message.push_str("\r\n");
	}

	Err!("SMTP client closed during message data")
}
