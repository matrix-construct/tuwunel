use std::sync::Arc;

use itertools::Itertools;
use ruma::{
	EventId, RoomId, UserId,
	events::{
		relation::InReplyTo,
		room::message::{Relation, RoomMessageEventContent},
	},
};
use tracing::Level;
use tuwunel_core::{
	log::{
		capture::EventData,
		fmt::{markdown_table, markdown_table_head},
	},
	pdu::PduBuilder,
};

use super::Service;
use crate::command::{CommandResult, CommandSystem};

impl Service {
	pub fn run_command_matrix_detached(
		&self,
		command_system: &Arc<dyn CommandSystem>,
		reply_id: &EventId,
		room_id: &RoomId,
		message: &str,
		sender: &UserId,
		reply_sender: &UserId,
		capture_level: Option<Level>,
	) {
		debug_assert!(
			self.services.globals.user_is_local(reply_sender),
			"reply sender not local"
		);

		let this = self.services.command.clone();
		let command_system = command_system.clone();
		let reply_id = reply_id.to_owned();
		let room_id = room_id.to_owned();
		let message = message.to_owned();
		let sender = sender.to_owned();
		let reply_sender = reply_sender.to_owned();

		let f = || async move {
			// let e = e;
			this.run_matrix_command(
				command_system.as_ref(),
				&reply_id,
				&room_id,
				&message,
				&sender,
				&reply_sender,
				capture_level,
			)
			.await;
		};

		self.services.server.runtime().spawn(f());
	}

	pub async fn run_matrix_command(
		&self,
		command_system: &dyn CommandSystem,
		reply_id: &EventId,
		room_id: &RoomId,
		message: &str,
		sender: &UserId,
		reply_sender: &UserId,
		capture_level: Option<Level>,
	) {
		debug_assert!(
			self.services.globals.user_is_local(reply_sender),
			"reply sender not local"
		);

		let mut lines = message.lines();
		let command = lines.next().unwrap();
		let input = lines.join("\n");
		let result = self
			.services
			.command
			.run_command(command_system, command, &input, Some(sender))
			.await;
		self.send_response(result, reply_id, room_id, reply_sender, capture_level)
			.await;
	}

	async fn send_response(
		&self,
		result: CommandResult,
		reply_id: &EventId,
		room_id: &RoomId,
		reply_sender: &UserId,
		capture_level: Option<Level>,
	) {
		let mut output = String::new();
		if let Some(capture_level) = capture_level {
			output.push_str(&format_logs(&result.logs, capture_level));
		}
		if !result.output.starts_with("Usage:") {
			if result.err {
				output.push_str("Command completed with error:\n");
			} else {
				output.push_str("Command completed:\n");
			}
		}
		output.push_str(&result.output);
		let mut content = RoomMessageEventContent::notice_markdown(output);
		content.relates_to = Some(Relation::Reply {
			in_reply_to: InReplyTo { event_id: reply_id.to_owned() },
		});

		let state_lock = self.services.state.mutex.lock(room_id).await;

		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::timeline(&content),
				reply_sender,
				room_id,
				&state_lock,
			)
			.await
			.expect("sending command response failed");
	}
}

fn format_logs(events: &[EventData], level: Level) -> String {
	if events.is_empty() {
		return String::new();
	}

	let mut logs = String::new();
	markdown_table_head(&mut logs).expect("markdown table header");

	events
		.iter()
		.filter(|event_data| event_data.level <= level)
		.for_each(|event_data| {
			markdown_table(
				&mut logs,
				&event_data.level,
				event_data.span_name,
				&event_data
					.message()
					.escape_default()
					.collect::<String>(),
			)
			.expect("log line appended");
		});

	logs.push('\n');

	logs
}
