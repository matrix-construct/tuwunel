use std::{
	borrow::Cow,
	collections::BTreeMap,
	fmt::{Result as FmtResult, Write as _},
	num::NonZeroUsize,
	time::{Duration, Instant},
};

use futures::StreamExt;
use ruma::{
	OwnedRoomOrAliasId,
	api::federation::discovery::get_server_version::v1::{Request, Response, Server},
};
use tuwunel_core::{Result, utils::time::Elapsed};
use tuwunel_service::federation::feds::{Fault, Outcome};

use super::{SweepArgs, fault_message, markdown_cell, prepare, render_total_time};
use crate::admin_command;

pub(super) const WIDTH_DEFAULT: NonZeroUsize = NonZeroUsize::new(192).expect("192 is nonzero");

#[derive(Default, Eq, Ord, PartialEq, PartialOrd)]
struct Version {
	name: Option<String>,
	version: Option<String>,
	compiler: Option<String>,
	kernel: Option<String>,
	arch: Option<String>,
}

type ClassCounts<'a> = BTreeMap<&'a Version, usize>;
type ClassNumbers<'a> = BTreeMap<&'a Version, usize>;
type VersionOutcome = Outcome<Option<Version>>;

#[derive(Clone, Copy)]
enum ListMode {
	None,
	Successes,
	All,
	Errors,
}

#[admin_command]
pub(super) async fn feds_version(
	&self,
	room: OwnedRoomOrAliasId,
	list: bool,
	list_all: bool,
	list_errors: bool,
	sweep: SweepArgs,
) -> Result {
	let prepared = prepare(self, &room, sweep, WIDTH_DEFAULT).await?;
	let started = Instant::now();
	let outcomes = self
		.services
		.federation
		.for_room(&prepared.room_id, |_| Request::new(), prepared.opts)
		.map(|outcome| Outcome {
			origin: outcome.origin,
			elapsed: outcome.elapsed,
			result: outcome.result.map(into_version),
		})
		.collect::<Vec<_>>()
		.await;

	let total = started.elapsed();

	let list_mode = match (list, list_all, list_errors) {
		| (true, false, false) => ListMode::Successes,
		| (false, true, false) => ListMode::All,
		| (false, false, true) => ListMode::Errors,
		| _ => ListMode::None,
	};

	let output = render(outcomes, total, list_mode);

	self.write_str(&output).await
}

fn into_version(response: Response) -> Option<Version> {
	response.server.map(|server| {
		let Server {
			name, version, compiler, kernel, arch, ..
		} = server;

		Version { name, version, compiler, kernel, arch }
	})
}

fn render(mut outcomes: Vec<VersionOutcome>, total: Duration, list_mode: ListMode) -> String {
	outcomes.sort_by(|left, right| left.origin.cmp(&right.origin));

	let mut output = String::new();

	render_into(&mut output, &outcomes, total, list_mode)
		.expect("writing to a String cannot fail");

	output
}

fn render_into(
	output: &mut String,
	outcomes: &[VersionOutcome],
	total: Duration,
	list_mode: ListMode,
) -> FmtResult {
	let counts: ClassCounts<'_> = outcomes
		.iter()
		.filter_map(|outcome| {
			outcome
				.result
				.as_ref()
				.ok()
				.and_then(Option::as_ref)
		})
		.fold(BTreeMap::new(), |mut classes, version| {
			classes
				.entry(version)
				.and_modify(|count| *count = count.saturating_add(1))
				.or_insert(1);

			classes
		});

	let mut classes: Vec<_> = counts
		.into_iter()
		.enumerate()
		.map(|(class, (version, count))| (class.saturating_add(1), version, count))
		.collect();

	let class_numbers: ClassNumbers<'_> = classes
		.iter()
		.map(|(class, version, _)| (*version, *class))
		.collect();

	classes.sort_unstable_by(|(_, left_version, left_count), (_, right_version, right_count)| {
		right_count
			.cmp(left_count)
			.then_with(|| left_version.cmp(right_version))
	});

	writeln!(output, "| rank | class | servers | name | version | compiler | kernel | arch |")?;
	writeln!(output, "| ---: | ----: | ------: | :--- | :--- | :--- | :--- | :--- |",)?;

	for (rank, (class, version, count)) in classes.iter().enumerate() {
		writeln!(
			output,
			"| {} | {} | {} | {} | {} | {} | {} | {} |",
			rank.saturating_add(1),
			class,
			count,
			option_cell(version.name.as_deref()),
			option_cell(version.version.as_deref()),
			option_cell(version.compiler.as_deref()),
			option_cell(version.kernel.as_deref()),
			option_cell(version.arch.as_deref()),
		)?;
	}

	if matches!(list_mode, ListMode::None) {
		return render_total_time(output, total);
	}

	writeln!(output, "\n| origin | class | elapsed | fault |")?;
	writeln!(output, "| :--- | ----: | ---: | :--- |")?;
	for outcome in outcomes.iter().filter(|outcome| match list_mode {
		| ListMode::None => false,
		| ListMode::Successes => outcome.result.is_ok(),
		| ListMode::All => true,
		| ListMode::Errors => outcome.result.is_err(),
	}) {
		match &outcome.result {
			| Ok(Some(version)) => writeln!(
				output,
				"| {} | {} | {} | |",
				outcome.origin,
				class_numbers
					.get(version)
					.copied()
					.unwrap_or_default(),
				Elapsed::from(outcome.elapsed),
			)?,
			| Ok(None) => writeln!(
				output,
				"| {} | | {} | missing server metadata |",
				outcome.origin,
				Elapsed::from(outcome.elapsed),
			)?,
			| Err(fault @ Fault::NotAttempted) => writeln!(
				output,
				"| {} | | | {} |",
				outcome.origin,
				markdown_cell(&fault_message(fault)),
			)?,
			| Err(fault) => writeln!(
				output,
				"| {} | | {} | {} |",
				outcome.origin,
				Elapsed::from(outcome.elapsed),
				markdown_cell(&fault_message(fault)),
			)?,
		}
	}

	render_total_time(output, total)
}

fn option_cell(value: Option<&str>) -> Cow<'_, str> {
	value
		.map(markdown_cell)
		.unwrap_or(Cow::Borrowed(""))
}

#[cfg(test)]
mod tests {
	use ruma::{ServerName, server_name};

	use super::*;

	#[test]
	fn render_ranks_classes_without_renumbering_and_leaves_missing_metadata_blank() {
		let outcomes = vec![
			success(server_name!("rare.example"), "alpha"),
			success(server_name!("popular-a.example"), "zeta"),
			success(server_name!("popular-b.example"), "zeta"),
			Outcome {
				origin: server_name!("skipped.example").to_owned(),
				elapsed: Duration::ZERO,
				result: Err(Fault::NotAttempted),
			},
		];

		let output = render(outcomes, Duration::ZERO, ListMode::All);
		let popular = output
			.find("| 1 | 2 | 2 | zeta |  |  |  |  |")
			.expect("popular class should be rendered first");

		let rare = output
			.find("| 2 | 1 | 1 | alpha |  |  |  |  |")
			.expect("rare class should be rendered second");

		assert!(popular < rare, "larger classes should precede smaller classes");
		assert_eq!(option_cell(None), "", "missing metadata should render blank");
		assert!(
			output.contains("| popular-a.example | 2 |"),
			"origin rows should keep the stable class number",
		);
		assert!(
			output.contains("| rare.example | 1 |"),
			"origin rows should keep the stable class number",
		);

		assert!(
			output.contains("| skipped.example | | | sweep budget exhausted before dispatch |")
		);
	}

	#[test]
	fn detail_listing_is_opt_in_and_filters_by_request_result() {
		let outcomes = || {
			vec![success(server_name!("good.example"), "alpha"), Outcome {
				origin: server_name!("bad.example").to_owned(),
				elapsed: Duration::from_secs(1),
				result: Err(Fault::Elapsed),
			}]
		};

		let summary = render(outcomes(), Duration::ZERO, ListMode::None);

		assert!(!summary.contains("| origin |"));

		let successes = render(outcomes(), Duration::ZERO, ListMode::Successes);

		assert!(successes.contains("good.example"));
		assert!(!successes.contains("bad.example"));

		let errors = render(outcomes(), Duration::ZERO, ListMode::Errors);

		assert!(!errors.contains("good.example"));
		assert!(errors.contains("bad.example"));
	}

	fn success(origin: &ServerName, name: &str) -> VersionOutcome {
		let version = Version {
			name: Some(name.to_owned()),
			..Default::default()
		};

		Outcome {
			origin: origin.to_owned(),
			elapsed: Duration::ZERO,
			result: Ok(Some(version)),
		}
	}
}
