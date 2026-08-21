use std::{
	cmp::Ordering,
	collections::BTreeMap,
	fmt::{Result as FmtResult, Write as _},
	num::NonZeroUsize,
	time::{Duration, Instant},
};

use futures::StreamExt;
use ruma::{
	OwnedEventId, OwnedRoomOrAliasId, OwnedUserId, UInt, UserId,
	api::{
		error::ErrorKind,
		federation::membership::prepare_join_event::v1::{Request, Response},
	},
};
use serde::Deserialize;
use smallvec::SmallVec;
use tuwunel_core::{Err, Error, Result, utils::time::Elapsed};
use tuwunel_service::federation::feds::{Fault, Outcome};

use super::{
	SweepArgs, fault_message, markdown_cell, prepare, render_total_time,
	sorted_event_id_difference,
};
use crate::admin_command;

pub(super) const WIDTH_DEFAULT: NonZeroUsize = NonZeroUsize::new(192).expect("192 is nonzero");

type Extremities = SmallVec<[OwnedEventId; 1]>;

#[derive(Deserialize)]
struct Head {
	depth: UInt,
	prev_events: Extremities,
}

type Classes<'a> = BTreeMap<&'a [OwnedEventId], usize>;
type ClassNumbers<'a> = BTreeMap<&'a [OwnedEventId], usize>;

#[admin_command]
pub(super) async fn feds_head(
	&self,
	room: OwnedRoomOrAliasId,
	probe_user: Option<OwnedUserId>,
	sweep: SweepArgs,
) -> Result {
	let prepared = prepare(self, &room, sweep, WIDTH_DEFAULT).await?;
	let probe_user = match probe_user {
		| Some(probe_user) => probe_user,
		| None =>
			UserId::parse_with_server_name("_feds_probe", self.services.globals.server_name())?,
	};

	if !self.services.globals.user_is_local(&probe_user) {
		return Err!("The feds head probe user must be local.");
	}

	let latest = self
		.services
		.timeline
		.latest_pdu_in_room(&prepared.room_id)
		.await?;

	let local_depth = latest.depth;
	let local_extremities = self
		.services
		.state
		.get_forward_extremities(&prepared.room_id)
		.map(ToOwned::to_owned)
		.collect::<Extremities>()
		.await;

	let local_extremities = normalize(local_extremities);
	let versions: Vec<_> = self
		.services
		.config
		.supported_room_versions()
		.map(|(version, _stability)| version)
		.collect();

	let request_room = prepared.room_id.clone();
	let started = Instant::now();
	let outcomes = self
		.services
		.federation
		.for_room(
			&prepared.room_id,
			move |_| Request {
				room_id: request_room.clone(),
				user_id: probe_user.clone(),
				ver: versions.clone(),
			},
			prepared.opts,
		)
		.map(parse_outcome)
		.collect::<Vec<_>>()
		.await;

	let total = started.elapsed();

	let output = render(outcomes, local_depth, &local_extremities, total);

	self.write_str(&output).await
}

fn normalize(mut event_ids: Extremities) -> Extremities {
	event_ids.sort_unstable();
	event_ids.dedup();

	event_ids
}

fn parse_outcome(outcome: Outcome<Response>) -> Outcome<Head> {
	let result = outcome.result.and_then(|response| {
		serde_json::from_str(response.event.get())
			.map_err(Error::from)
			.map_err(Fault::Error)
			.map(normalize_head)
	});

	Outcome {
		origin: outcome.origin,
		elapsed: outcome.elapsed,
		result,
	}
}

fn normalize_head(mut head: Head) -> Head {
	head.prev_events = normalize(head.prev_events);

	head
}

fn render(
	mut outcomes: Vec<Outcome<Head>>,
	local_depth: UInt,
	local_extremities: &[OwnedEventId],
	total: Duration,
) -> String {
	outcomes.sort_by(|left, right| match (&left.result, &right.result) {
		| (Err(_), Err(_)) => left.origin.cmp(&right.origin),
		| (Ok(_), Err(_)) => Ordering::Less,
		| (Err(_), Ok(_)) => Ordering::Greater,
		| (Ok(left_head), Ok(right_head)) => right_head
			.depth
			.cmp(&left_head.depth)
			.then_with(|| left.origin.cmp(&right.origin)),
	});

	let mut output = String::new();

	render_into(&mut output, &outcomes, local_depth, local_extremities, total)
		.expect("writing to a String cannot fail");

	output
}

fn render_into(
	output: &mut String,
	outcomes: &[Outcome<Head>],
	local_depth: UInt,
	local_extremities: &[OwnedEventId],
	total: Duration,
) -> FmtResult {
	let classes: Classes<'_> = outcomes
		.iter()
		.filter_map(|outcome| {
			outcome
				.result
				.as_ref()
				.ok()
				.map(|head| head.prev_events.as_slice())
		})
		.fold(BTreeMap::new(), |mut classes, extremities| {
			classes
				.entry(extremities)
				.and_modify(|count| *count = count.saturating_add(1))
				.or_insert(1);

			classes
		});

	let class_numbers: ClassNumbers<'_> = classes
		.keys()
		.copied()
		.enumerate()
		.map(|(class, extremities)| (extremities, class.saturating_add(1)))
		.collect();

	let incompatible = outcomes
		.iter()
		.filter(|outcome| {
			matches!(
				&outcome.result,
				Err(Fault::Error(error))
					if matches!(
						error.kind(),
						ErrorKind::IncompatibleRoomVersion { .. }
							| ErrorKind::UnsupportedRoomVersion
					)
			)
		})
		.count();

	writeln!(
		output,
		"Remote template depth is one greater than the selected room head. {incompatible} \
		 servers reported an incompatible room version.\n"
	)?;

	writeln!(output, "| origin | depth | extremities | class | elapsed | fault |")?;
	writeln!(output, "| :--- | ---: | ---: | ----: | ---: | :--- |")?;
	writeln!(output, "| local | {local_depth} | {} | local | | |", local_extremities.len(),)?;
	for outcome in outcomes {
		match &outcome.result {
			| Ok(head) => writeln!(
				output,
				"| {} | {} | {} | {} | {} | |",
				outcome.origin,
				head.depth,
				head.prev_events.len(),
				class_numbers
					.get(head.prev_events.as_slice())
					.copied()
					.unwrap_or_default(),
				Elapsed::from(outcome.elapsed),
			)?,
			| Err(fault @ Fault::NotAttempted) => writeln!(
				output,
				"| {} | | | | | {} |",
				outcome.origin,
				markdown_cell(&fault_message(fault)),
			)?,
			| Err(fault) => writeln!(
				output,
				"| {} | | | | {} | {} |",
				outcome.origin,
				Elapsed::from(outcome.elapsed),
				markdown_cell(&fault_message(fault)),
			)?,
		}
	}

	writeln!(output, "\n### Extremity equivalence classes\n")?;
	writeln!(output, "| class | servers | size | vs local |")?;
	writeln!(output, "| ----: | ------: | ---: | :--- |")?;
	for (class, (extremities, count)) in classes.iter().enumerate() {
		writeln!(
			output,
			"| {} | {} | {} | {} |",
			class.saturating_add(1),
			count,
			extremities.len(),
			sorted_event_id_difference(extremities, local_extremities),
		)?;
	}

	for (class, extremities) in classes.keys().enumerate() {
		writeln!(output, "\n#### Extremity class {}\n\n```", class.saturating_add(1))?;
		for event_id in *extremities {
			writeln!(output, "{event_id}")?;
		}

		writeln!(output, "```")?;
	}

	render_total_time(output, total)
}

#[cfg(test)]
mod tests {
	use ruma::server_name;

	use super::*;

	#[test]
	fn undispatched_destination_has_no_elapsed_time() {
		let outcomes: Vec<Outcome<Head>> = vec![Outcome {
			origin: server_name!("skipped.example").to_owned(),
			elapsed: Duration::ZERO,
			result: Err(Fault::NotAttempted),
		}];

		let output = render(outcomes, UInt::from(1_u8), &[], Duration::ZERO);

		assert!(
			output
				.contains("| skipped.example | | | | | sweep budget exhausted before dispatch |")
		);
	}
}
