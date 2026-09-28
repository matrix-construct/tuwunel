use std::{
	fmt::{Result as FmtResult, Write as _},
	time::Instant,
};

use ruma::OwnedRoomId;
use tuwunel_core::{
	Result,
	itertools::Itertools,
	utils::{
		string::{markdown_cell, plural},
		time::Elapsed,
	},
};
use tuwunel_service::rooms::event_handler::{InFlightWalk, Walk};

use crate::admin_command;

#[admin_command]
pub(super) async fn incoming_federation(&self) -> Result {
	let event_handler = &self.services.event_handler;
	let locked = event_handler.mutex_federation.keys();
	let walks: Vec<_> = event_handler.prev_walks_in_flight().collect();
	let now = Instant::now();
	let output = render(&walks, unwalked(locked, &walks), now);

	self.write_str(&output).await
}

fn unwalked(
	locked: impl IntoIterator<Item = OwnedRoomId>,
	walks: &[InFlightWalk],
) -> impl ExactSizeIterator<Item = OwnedRoomId> {
	locked
		.into_iter()
		.filter(|room_id| walks.iter().all(|walk| walk.room_id.ne(room_id)))
		.sorted_unstable()
}

fn render(
	walks: &[InFlightWalk],
	unwalked: impl ExactSizeIterator<Item = OwnedRoomId>,
	now: Instant,
) -> String {
	let mut output = String::new();

	render_into(&mut output, walks, unwalked, now).expect("writing to a String cannot fail");

	output
}

fn render_into(
	output: &mut String,
	walks: &[InFlightWalk],
	unwalked: impl ExactSizeIterator<Item = OwnedRoomId>,
	now: Instant,
) -> FmtResult {
	write_walks(output, walks, now)?;
	write_unwalked(output, unwalked)
}

fn write_walks(output: &mut String, walks: &[InFlightWalk], now: Instant) -> FmtResult {
	let noun = plural(walks.len(), "prev walk", "prev walks");

	writeln!(output, "{} {noun} in flight.", walks.len())?;
	if walks.is_empty() {
		return Ok(());
	}

	writeln!(output, "\n| room | event | origin | phase | elapsed | fetch | prevs | capped |")?;
	writeln!(output, "| :--- | :--- | :--- | :--- | ---: | ---: | ---: | :--- |")?;
	for walk in walks {
		write_walk(output, walk, now)?;
	}

	Ok(())
}

fn write_walk(output: &mut String, in_flight: &InFlightWalk, now: Instant) -> FmtResult {
	let InFlightWalk { room_id, event_id, origin, started, walk } = in_flight;
	let elapsed = Elapsed::from(now.saturating_duration_since(*started));

	write!(
		output,
		"| {} | {} | {} |",
		markdown_cell(room_id.as_str()),
		markdown_cell(event_id.as_str()),
		markdown_cell(origin.as_str()),
	)?;

	match walk {
		| None => writeln!(output, " fetch | {elapsed} | | | |"),
		| Some(Walk { fetched, prevs, capped }) => {
			let fetch = Elapsed::from(fetched.saturating_duration_since(*started));
			let capped = if *capped { "yes" } else { "no" };

			writeln!(output, " walk | {elapsed} | {fetch} | {prevs} | {capped} |")
		},
	}
}

fn write_unwalked(
	output: &mut String,
	unwalked: impl ExactSizeIterator<Item = OwnedRoomId>,
) -> FmtResult {
	let rooms = unwalked.len();
	let noun = plural(rooms, "room", "rooms");

	writeln!(output, "\n{rooms} {noun} holding the federation lock with no walk.")?;
	if rooms == 0 {
		return Ok(());
	}

	writeln!(output, "\n| room |\n| :--- |")?;
	for room_id in unwalked {
		writeln!(output, "| {} |", markdown_cell(room_id.as_str()))?;
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use ruma::{owned_event_id, owned_room_id, owned_server_name};

	use super::*;

	#[test]
	fn render_lists_walks_then_rooms_without_one() {
		let started = Instant::now();
		let walk = Walk {
			fetched: after(started, 850),
			prevs: 37,
			capped: true,
		};

		let walks = [
			InFlightWalk {
				room_id: owned_room_id!("!fetching:example.org"),
				event_id: owned_event_id!("$fetch"),
				origin: owned_server_name!("other.example"),
				started: after(started, 12_830),
				walk: None,
			},
			InFlightWalk {
				room_id: owned_room_id!("!walking:example.org"),
				event_id: owned_event_id!("$walk"),
				origin: owned_server_name!("remote.example"),
				started,
				walk: Some(walk),
			},
		];

		let locked =
			[owned_room_id!("!walking:example.org"), owned_room_id!("!idle:example.org")];

		let output = render(&walks, unwalked(locked, &walks), after(started, 14_030));
		let (_, rooms) = output
			.split_once("holding the federation lock")
			.expect("the rooms without a walk are counted");

		assert!(rooms.contains("| !idle:example.org |"), "an idle locked room must be listed");
		assert!(
			!rooms.contains("!walking:example.org"),
			"a room with a walk must not be listed again"
		);

		assert_eq!(output.lines().collect::<Vec<_>>(), [
			"2 prev walks in flight.",
			"",
			"| room | event | origin | phase | elapsed | fetch | prevs | capped |",
			"| :--- | :--- | :--- | :--- | ---: | ---: | ---: | :--- |",
			"| !fetching:example.org | $fetch | other.example | fetch | 1.2s | | | |",
			"| !walking:example.org | $walk | remote.example | walk | 14.03s | 850ms | 37 | yes \
			 |",
			"",
			"1 room holding the federation lock with no walk.",
			"",
			"| room |",
			"| :--- |",
			"| !idle:example.org |",
		]);
	}

	#[test]
	fn render_omits_empty_tables() {
		let output = render(&[], unwalked([], &[]), Instant::now());

		assert_eq!(
			output,
			"0 prev walks in flight.\n\n0 rooms holding the federation lock with no walk.\n"
		);
	}

	fn after(started: Instant, millis: u64) -> Instant {
		started
			.checked_add(Duration::from_millis(millis))
			.expect("the test instant is in range")
	}
}
