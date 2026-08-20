use std::{
	borrow::Cow,
	collections::BTreeMap,
	fmt::{Error as FmtError, Write as _},
	iter::once,
	time::{Duration, Instant},
};

use futures::StreamExt;
use ruma::{
	OwnedEventId, OwnedRoomOrAliasId, OwnedServerName, ServerName,
	api::federation::event::get_room_state_ids::v1::Request,
};
use tuwunel_core::Result as CoreResult;
use tuwunel_service::federation::feds::{Fault, Grid, Origins, OutcomeExt};

use super::{
	SweepArgs, fault_message, markdown_cell, prepare, render_total_time,
	sorted_event_id_difference,
};
use crate::admin_command;

type RenderResult<T = ()> = Result<T, FmtError>;
type SetClass<'a> = (&'a [OwnedEventId], &'a Origins);
type OriginClasses<'a> = BTreeMap<&'a ServerName, usize>;

struct Render<'a> {
	event_id: &'a OwnedEventId,
	grid: &'a Grid<StateSet>,
	local_state: &'a [OwnedEventId],
	auth_chain: bool,
	full: bool,
	total: Duration,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum StateSet {
	State(Vec<OwnedEventId>),
	Auth(Vec<OwnedEventId>),
}

#[derive(Clone, Copy)]
enum SetKind {
	State,
	Auth,
}

enum OriginRow<'a> {
	Class(usize),
	Empty,
	Fault(&'a Fault),
}

#[admin_command]
pub(super) async fn feds_state(
	&self,
	room: OwnedRoomOrAliasId,
	at: Option<OwnedEventId>,
	auth_chain: bool,
	full: bool,
	sweep: SweepArgs,
) -> CoreResult {
	let prepared = prepare(self, &room, sweep).await?;
	let event_id = match at {
		| Some(event_id) => event_id,
		| None =>
			self.services
				.timeline
				.latest_pdu_in_room(&prepared.room_id)
				.await?
				.event_id,
	};

	let shortstatehash = self
		.services
		.state
		.pdu_shortstatehash(&event_id)
		.await?;

	let local_state = self
		.services
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(|(_shortstatekey, event_id)| event_id)
		.collect::<Vec<_>>()
		.await;

	let local_state = normalize(local_state);
	let request_room = prepared.room_id.clone();
	let request_event = event_id.clone();
	let started = Instant::now();
	let grid = self
		.services
		.federation
		.for_room(
			&prepared.room_id,
			move |_| Request {
				room_id: request_room.clone(),
				event_id: request_event.clone(),
			},
			prepared.opts,
		)
		.grid(move |response| {
			let state = StateSet::State(normalize(response.pdu_ids));
			let auth = auth_chain.then(|| StateSet::Auth(normalize(response.auth_chain_ids)));

			once(state).chain(auth)
		})
		.await;

	let total = started.elapsed();

	let output = render(&event_id, &grid, &local_state, auth_chain, full, total);

	self.write_str(&output).await
}

fn normalize(mut event_ids: Vec<OwnedEventId>) -> Vec<OwnedEventId> {
	event_ids.sort_unstable();
	event_ids.dedup();

	event_ids
}

fn render(
	event_id: &OwnedEventId,
	grid: &Grid<StateSet>,
	local_state: &[OwnedEventId],
	auth_chain: bool,
	full: bool,
	total: Duration,
) -> String {
	let mut output = String::new();
	let args = Render {
		event_id,
		grid,
		local_state,
		auth_chain,
		full,
		total,
	};

	render_into(&mut output, args).expect("writing to a String cannot fail");

	output
}

fn render_into(
	output: &mut String,
	Render {
		event_id,
		grid,
		local_state,
		auth_chain,
		full,
		total,
	}: Render<'_>,
) -> RenderResult {
	writeln!(output, "State identifiers before `{event_id}`.")?;

	let state_origins = render_classes(
		output,
		"State",
		classes_for(grid, SetKind::State),
		Some(local_state),
		full,
	)?;

	let auth_origins = auth_chain
		.then(|| {
			render_classes(output, "Auth chain", classes_for(grid, SetKind::Auth), None, full)
		})
		.transpose()?;

	let rows: BTreeMap<OwnedServerName, OriginRow<'_>> = state_origins
		.iter()
		.map(|(origin, class)| ((*origin).to_owned(), OriginRow::Class(*class)))
		.chain(
			grid.empty
				.iter()
				.cloned()
				.map(|origin| (origin, OriginRow::Empty)),
		)
		.chain(
			grid.faults
				.iter()
				.map(|(origin, fault)| (origin.clone(), OriginRow::Fault(fault))),
		)
		.collect();

	writeln!(output, "\n| origin | state class | auth class | fault |")?;
	writeln!(output, "| :--- | ----: | ----: | :--- |")?;
	for (origin, row) in rows {
		match row {
			| OriginRow::Empty => writeln!(output, "| {origin} | | | empty response |")?,
			| OriginRow::Fault(fault) =>
				writeln!(output, "| {origin} | | | {} |", markdown_cell(&fault_message(fault)),)?,
			| OriginRow::Class(class) => {
				let auth_class = auth_origins
					.as_ref()
					.and_then(|classes| classes.get::<ServerName>(origin.as_ref()));

				match auth_class {
					| None => writeln!(output, "| {origin} | {class} | | |")?,
					| Some(auth_class) =>
						writeln!(output, "| {origin} | {class} | {auth_class} | |")?,
				}
			},
		}
	}

	render_total_time(output, total)
}

fn classes_for(
	grid: &Grid<StateSet>,
	kind: SetKind,
) -> impl Iterator<Item = SetClass<'_>> + Clone {
	grid.data
		.iter()
		.filter_map(move |(set, origins)| match (set, kind) {
			| (StateSet::State(ids), SetKind::State) | (StateSet::Auth(ids), SetKind::Auth) =>
				Some((ids.as_slice(), origins)),
			| _ => None,
		})
}

fn render_classes<'a, I>(
	output: &mut String,
	title: &str,
	classes: I,
	local: Option<&[OwnedEventId]>,
	full: bool,
) -> RenderResult<OriginClasses<'a>>
where
	I: Iterator<Item = SetClass<'a>> + Clone,
{
	writeln!(output, "\n### {title} equivalence classes\n")?;
	writeln!(output, "| class | servers | size | vs ours |")?;
	writeln!(output, "| ----: | ------: | ---: | :--- |")?;
	for (class, (ids, origins)) in classes.clone().enumerate() {
		let comparison =
			local.map_or(Cow::Borrowed("n/a"), |local| sorted_event_id_difference(ids, local));

		writeln!(
			output,
			"| {} | {} | {} | {comparison} |",
			class.saturating_add(1),
			origins.len(),
			ids.len(),
		)?;
	}

	if full {
		for (class, (ids, _origins)) in classes.clone().enumerate() {
			writeln!(output, "\n#### {title} class {}\n\n```", class.saturating_add(1))?;
			for event_id in ids {
				writeln!(output, "{event_id}")?;
			}

			writeln!(output, "```")?;
		}
	}

	let origin_classes: OriginClasses<'_> = classes
		.enumerate()
		.flat_map(|(class, (_ids, origins))| {
			origins
				.iter()
				.map(move |origin| (origin.as_ref(), class.saturating_add(1)))
		})
		.collect();

	Ok(origin_classes)
}
