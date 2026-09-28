use std::{
	cmp::Ordering,
	fmt::Write,
	time::{Duration, Instant},
};

use futures::StreamExt;
use ruma::{OwnedRoomId, RoomId, ServerName};
use tuwunel_core::{
	Result,
	itertools::Itertools,
	utils::{
		BoolExt,
		math::usize_from_u64_truncated,
		string::{collect_stream, markdown_cell, plural},
		time::{Elapsed, format as format_time},
	},
};
use tuwunel_service::rooms::event_handler::{PrevWalkOutcome, PrevWalkPass, PrevWalkRoom};

use crate::admin_command;

const ROOMS_HEAD: &str = "| room | passes | appended | not appended | failed | cancelled | \
                          fetch failed | fetch cancelled | capped | prevs | unprocessed | fetch \
                          | upgrade |\n| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: \
                          | ---: | ---: | ---: | ---: | ---: |";

const PASSES_HEAD: &str = "| ended | event | origin | outcome | prevs | unprocessed | capped | \
                           fetch | upgrade |\n| :--- | :--- | :--- | :--- | ---: | ---: | :--- \
                           | ---: | ---: |";

#[admin_command]
pub(super) async fn prev_walk_rooms(&self, room_id: Option<OwnedRoomId>, limit: usize) -> Result {
	let event_handler = &self.services.event_handler;
	let output = match room_id {
		| None => {
			let started = Instant::now();
			let rooms = event_handler.prev_walk_rooms().await;

			render_rooms(rooms, limit, started.elapsed())?
		},
		| Some(room_id) => {
			let passes: Vec<_> = event_handler
				.prev_walk_passes(&room_id)
				.take(limit)
				.collect()
				.await;

			collect_stream(|out| write_passes(out, &room_id, &passes))?
		},
	};

	self.write_str(&output).await
}

/// Ranks the rooms busiest first and renders the top `limit`.
///
/// The header counts every room swept and the footer every pass, whatever the
/// limit leaves out.
fn render_rooms(
	rooms: impl ExactSizeIterator<Item = PrevWalkRoom>,
	limit: usize,
	sweep: Duration,
) -> Result<String> {
	let count = rooms.len();
	let sorted = rooms.sorted_unstable_by(busiest_first);
	let ranked = sorted.as_slice();
	let passes = ranked
		.iter()
		.fold(0, |passes: u64, room| passes.saturating_add(room.passes));

	let top = ranked.get(..limit).unwrap_or(ranked);

	collect_stream(|out| write_rooms(out, count, passes, top, sweep))
}

fn busiest_first(a: &PrevWalkRoom, b: &PrevWalkRoom) -> Ordering {
	b.passes
		.cmp(&a.passes)
		.then_with(|| a.room_id.cmp(&b.room_id))
}

fn write_rooms(
	out: &mut dyn Write,
	count: usize,
	passes: u64,
	top: &[PrevWalkRoom],
	sweep: Duration,
) -> Result {
	let noun = plural(count, "room", "rooms");

	writeln!(out, "{count} {noun} with recorded prev walks.")?;
	if !top.is_empty() {
		writeln!(out, "\n{ROOMS_HEAD}")?;
		top.iter()
			.try_for_each(|room| write_room(out, room))?;
	}

	let noun = plural(usize_from_u64_truncated(passes), "pass", "passes");

	writeln!(out, "\n{passes} {noun} in {}.", Elapsed::from(sweep))?;

	Ok(())
}

fn write_room(out: &mut dyn Write, room: &PrevWalkRoom) -> Result {
	let PrevWalkRoom {
		room_id,
		passes,
		appended,
		not_appended,
		failed,
		cancelled,
		fetch_failed,
		fetch_cancelled,
		capped,
		prevs,
		unprocessed,
		fetch,
		upgrade,
	} = room;

	writeln!(
		out,
		"| {} | {passes} | {appended} | {not_appended} | {failed} | {cancelled} | \
		 {fetch_failed} | {fetch_cancelled} | {capped} | {prevs} | {unprocessed} | {} | {} |",
		markdown_cell(room_id.as_str()),
		Elapsed::from(*fetch),
		Elapsed::from(*upgrade),
	)?;

	Ok(())
}

fn write_passes(out: &mut dyn Write, room_id: &RoomId, passes: &[PrevWalkPass]) -> Result {
	let count = passes.len();
	let noun = plural(count, "pass", "passes");

	writeln!(out, "{count} latest {noun} in {room_id}.")?;
	if count == 0 {
		return Ok(());
	}

	writeln!(out, "\n{PASSES_HEAD}")?;
	passes
		.iter()
		.try_for_each(|pass| write_pass(out, pass))
}

fn write_pass(out: &mut dyn Write, pass: &PrevWalkPass) -> Result {
	let PrevWalkPass {
		ended,
		event_id,
		origin,
		outcome,
		prevs,
		unprocessed,
		capped,
		fetch,
		upgrade,
	} = pass;

	let ended = format_time(*ended, "%+");
	let origin = origin.as_deref().map_or("", ServerName::as_str);
	let outcome = outcome.map_or("unknown", PrevWalkOutcome::name);
	let capped = capped.copy_or("no", "yes");

	writeln!(
		out,
		"| {ended} | {} | {} | {outcome} | {prevs} | {unprocessed} | {capped} | {} | {} |",
		markdown_cell(event_id.as_str()),
		markdown_cell(origin),
		Elapsed::from(*fetch),
		Elapsed::from(*upgrade),
	)?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use ruma::{owned_event_id, owned_server_name, room_id};
	use tuwunel_core::utils::time::timepoint_from_epoch;

	use super::*;

	#[test]
	fn render_rooms_lists_the_busiest_first() {
		let rooms = [
			PrevWalkRoom {
				passes: 1,
				appended: 1,
				..PrevWalkRoom::empty(room_id!("!aaa:example.org"))
			},
			PrevWalkRoom {
				passes: 3,
				not_appended: 1,
				failed: 1,
				fetch_failed: 1,
				prevs: 4,
				unprocessed: 2,
				fetch: Duration::from_millis(1_500),
				upgrade: Duration::from_millis(250),
				..PrevWalkRoom::empty(room_id!("!busy:example.org"))
			},
			PrevWalkRoom {
				passes: 3,
				cancelled: 2,
				fetch_cancelled: 1,
				capped: 1,
				prevs: 60,
				..PrevWalkRoom::empty(room_id!("!busier:example.org"))
			},
		];

		let output = render_rooms(rooms.into_iter(), 2, Duration::from_micros(1_500))
			.expect("the rooms render");

		assert_eq!(output.lines().collect::<Vec<_>>(), [
			"3 rooms with recorded prev walks.",
			"",
			"| room | passes | appended | not appended | failed | cancelled | fetch failed | \
			 fetch cancelled | capped | prevs | unprocessed | fetch | upgrade |",
			"| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | \
			 ---: | ---: |",
			"| !busier:example.org | 3 | 0 | 0 | 0 | 2 | 0 | 1 | 1 | 60 | 0 | 0ns | 0ns |",
			"| !busy:example.org | 3 | 0 | 1 | 1 | 0 | 1 | 0 | 0 | 4 | 2 | 1.5s | 250ms |",
			"",
			"7 passes in 1.5ms.",
		]);
	}

	#[test]
	fn render_rooms_omits_an_empty_table() {
		let output =
			render_rooms([].into_iter(), 20, Duration::from_millis(3)).expect("the rooms render");

		assert_eq!(output, "0 rooms with recorded prev walks.\n\n0 passes in 3ms.\n");
	}

	#[test]
	fn render_rooms_omits_the_table_at_limit_zero() {
		let rooms = [
			PrevWalkRoom {
				passes: 3,
				..PrevWalkRoom::empty(room_id!("!busy:example.org"))
			},
			PrevWalkRoom {
				passes: 1,
				..PrevWalkRoom::empty(room_id!("!quiet:example.org"))
			},
		];

		let output = render_rooms(rooms.into_iter(), 0, Duration::from_millis(1))
			.expect("the rooms render");

		assert_eq!(output, "2 rooms with recorded prev walks.\n\n4 passes in 1ms.\n");
	}

	#[test]
	fn write_passes_formats_each_pass() {
		let ended = |millis| {
			timepoint_from_epoch(Duration::from_millis(millis))
				.expect("the test time is in range")
		};

		let passes = [
			PrevWalkPass {
				ended: ended(1_700_000_000_250),
				event_id: owned_event_id!("$later"),
				origin: Some(owned_server_name!("remote.example")),
				outcome: Some(PrevWalkOutcome::Cancelled),
				prevs: 37,
				unprocessed: 0,
				capped: true,
				fetch: Duration::from_millis(850),
				upgrade: Duration::from_millis(14_030),
			},
			PrevWalkPass {
				ended: ended(1_700_000_000_000),
				event_id: owned_event_id!("$earlier"),
				origin: None,
				outcome: None,
				prevs: 0,
				unprocessed: 0,
				capped: false,
				fetch: Duration::from_millis(2),
				upgrade: Duration::ZERO,
			},
		];

		let room_id = room_id!("!room:example.org");
		let output =
			collect_stream(|out| write_passes(out, room_id, &passes)).expect("the passes render");

		assert_eq!(output.lines().collect::<Vec<_>>(), [
			"2 latest passes in !room:example.org.",
			"",
			"| ended | event | origin | outcome | prevs | unprocessed | capped | fetch | \
			 upgrade |",
			"| :--- | :--- | :--- | :--- | ---: | ---: | :--- | ---: | ---: |",
			"| 2023-11-14T22:13:20.250+00:00 | $later | remote.example | cancelled | 37 | 0 | \
			 yes | 850ms | 14.03s |",
			"| 2023-11-14T22:13:20+00:00 | $earlier |  | unknown | 0 | 0 | no | 2ms | 0ns |",
		]);
	}

	#[test]
	fn write_passes_omits_an_empty_table() {
		let room_id = room_id!("!room:example.org");
		let output =
			collect_stream(|out| write_passes(out, room_id, &[])).expect("the passes render");

		assert_eq!(output, "0 latest passes in !room:example.org.\n");
	}
}
