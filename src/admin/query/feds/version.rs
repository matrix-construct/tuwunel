use std::{
	borrow::Cow,
	collections::BTreeMap,
	fmt::{Result as FmtResult, Write as _},
	num::NonZeroUsize,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::ValueEnum;
use futures::{StreamExt, stream::iter};
use ruma::{
	OwnedRoomOrAliasId,
	api::federation::discovery::get_server_version::v1::{Request, Response, Server},
};
use tuwunel_core::{
	Result,
	itertools::Itertools,
	utils::{stream::ReadyExt, time::Elapsed},
};
use tuwunel_service::federation::feds::{Fault, Outcome};

use super::{SweepArgs, count_results, fault_message, markdown_cell, prepare, render_totals};
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
type VersionOutcome = Outcome<Option<Version>>;

#[derive(Clone, Copy)]
enum ListMode {
	None,
	Successes,
	All,
	Errors,
}

/// Column ordering the detail listing.
///
/// Rows equal under the chosen column keep their origin order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum Sort {
	/// Server name.
	#[default]
	Origin,

	/// Request latency, fastest first.
	Elapsed,

	/// Failure message.
	Fault,
}

#[admin_command]
pub(super) async fn feds_version(
	&self,
	room: OwnedRoomOrAliasId,
	list: bool,
	list_all: bool,
	list_errors: bool,
	sort: Sort,
	sweep: SweepArgs,
) -> Result {
	let prepared = prepare(self, &room, sweep, WIDTH_DEFAULT).await?;
	let backoffs = self.services.federation.peer_backoffs().await;
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs();

	let (eligible, outcomes) = self
		.services
		.state_cache
		.room_servers(&prepared.room_id)
		.ready_filter(|server| {
			!prepared.opts.exclude_self || !self.services.globals.server_is_ours(server)
		})
		.map(ToOwned::to_owned)
		.ready_fold((Vec::new(), Vec::new()), |(mut eligible, mut outcomes), origin| {
			let Some(backoff) = backoffs.get(&origin) else {
				eligible.push(origin);
				return (eligible, outcomes);
			};

			let retry_at = backoff
				.anchor_secs
				.saturating_add(backoff.delay_secs);

			if retry_at <= now {
				eligible.push(origin);
				return (eligible, outcomes);
			}

			outcomes.push(Outcome {
				origin,
				elapsed: Duration::ZERO,
				result: Err(Fault::Backoff {
					class: backoff.class,
					age: Duration::from_secs(now.saturating_sub(backoff.oldest_secs)),
					retry: Duration::from_secs(retry_at.saturating_sub(now)),
				}),
			});

			(eligible, outcomes)
		})
		.await;

	let started = Instant::now();
	let responses = self
		.services
		.federation
		.fanout_to(iter(eligible), |_| Request::new(), prepared.opts)
		.map(|outcome| Outcome {
			origin: outcome.origin,
			elapsed: outcome.elapsed,
			result: outcome.result.map(into_version),
		});

	let outcomes = iter(outcomes)
		.chain(responses)
		.collect::<Vec<_>>()
		.await;

	let total = started.elapsed();

	let list_mode = match (list, list_all, list_errors) {
		| (true, false, false) => ListMode::Successes,
		| (false, true, false) => ListMode::All,
		| (false, false, true) => ListMode::Errors,
		| _ => ListMode::None,
	};

	let output = render(outcomes, total, list_mode, sort);

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

fn render(
	outcomes: Vec<VersionOutcome>,
	total: Duration,
	list_mode: ListMode,
	sort: Sort,
) -> String {
	let outcomes = sorted(outcomes, sort);
	let mut output = String::new();

	render_into(&mut output, &outcomes, total, list_mode)
		.expect("writing to a String cannot fail");

	output
}

fn sorted(mut outcomes: Vec<VersionOutcome>, sort: Sort) -> Vec<VersionOutcome> {
	// Both secondary sorts are stable, so origin order remains the tie-breaker.
	outcomes.sort_by(|left, right| left.origin.cmp(&right.origin));
	match sort {
		| Sort::Origin => {},
		| Sort::Elapsed => outcomes.sort_by_key(|outcome| outcome.elapsed),
		| Sort::Fault => outcomes.sort_by_cached_key(fault_cell),
	}

	outcomes
}

fn fault_cell(outcome: &VersionOutcome) -> Cow<'static, str> {
	match &outcome.result {
		| Ok(Some(_)) => Cow::Borrowed(""),
		| Ok(None) => Cow::Borrowed("missing server metadata"),
		| Err(fault) => fault_message(fault),
	}
}

fn render_into(
	output: &mut String,
	outcomes: &[VersionOutcome],
	total: Duration,
	list_mode: ListMode,
) -> FmtResult {
	let results = count_results(outcomes);
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

	writeln!(output, "| rank | servers | name | version | compiler | kernel | arch |")?;
	writeln!(output, "| ---: | ------: | :--- | :--- | :--- | :--- | :--- |",)?;

	let classes = counts.into_iter().sorted_unstable_by(
		|(left_version, left_count), (right_version, right_count)| {
			right_count
				.cmp(left_count)
				.then_with(|| left_version.cmp(right_version))
		},
	);

	for (rank, (version, count)) in classes.enumerate() {
		writeln!(
			output,
			"| {} | {} | {} | {} | {} | {} | {} |",
			rank.saturating_add(1),
			count,
			option_cell(version.name.as_deref()),
			option_cell(version.version.as_deref()),
			option_cell(version.compiler.as_deref()),
			option_cell(version.kernel.as_deref()),
			option_cell(version.arch.as_deref()),
		)?;
	}

	if matches!(list_mode, ListMode::None) {
		return render_totals(output, results, total);
	}

	writeln!(output, "\n| origin | name | version | elapsed | fault |")?;
	writeln!(output, "| :--- | :--- | :--- | ---: | :--- |")?;
	for outcome in outcomes.iter().filter(|outcome| match list_mode {
		| ListMode::None => false,
		| ListMode::Successes => outcome.result.is_ok(),
		| ListMode::All => true,
		| ListMode::Errors => outcome.result.is_err(),
	}) {
		render_row(output, outcome)?;
	}

	render_totals(output, results, total)
}

fn render_row(output: &mut String, outcome: &VersionOutcome) -> FmtResult {
	let fault = fault_cell(outcome);

	match &outcome.result {
		| Ok(Some(version)) => writeln!(
			output,
			"| {} | {} | {} | {} | |",
			outcome.origin,
			option_cell(version.name.as_deref()),
			option_cell(version.version.as_deref()),
			Elapsed::from(outcome.elapsed),
		),
		| Err(Fault::NotAttempted | Fault::Backoff { .. }) =>
			writeln!(output, "| {} | | | | {} |", outcome.origin, markdown_cell(&fault)),
		| _ => writeln!(
			output,
			"| {} | | | {} | {} |",
			outcome.origin,
			Elapsed::from(outcome.elapsed),
			markdown_cell(&fault),
		),
	}
}

fn option_cell(value: Option<&str>) -> Cow<'_, str> {
	value
		.map(markdown_cell)
		.unwrap_or(Cow::Borrowed(""))
}

#[cfg(test)]
mod tests {
	use ruma::{ServerName, server_name};
	use tuwunel_service::federation::Classification;

	use super::*;

	#[test]
	fn render_ranks_versions_by_population_and_leaves_missing_metadata_blank() {
		let outcomes = vec![
			success(server_name!("rare.example"), "alpha"),
			success(server_name!("popular-a.example"), "zeta"),
			success(server_name!("popular-b.example"), "zeta"),
			Outcome {
				origin: server_name!("bare.example").to_owned(),
				elapsed: Duration::ZERO,
				result: Ok(None),
			},
			failed(server_name!("skipped.example"), Duration::ZERO, Fault::NotAttempted),
		];

		let output = render(outcomes, Duration::ZERO, ListMode::All, Sort::Origin);
		let popular = output
			.find("| 1 | 2 | zeta |  |  |  |  |")
			.expect("popular version should be rendered first");

		let rare = output
			.find("| 2 | 1 | alpha |  |  |  |  |")
			.expect("rare version should be rendered second");

		assert!(popular < rare, "more common versions should precede rarer versions");
		assert_eq!(option_cell(None), "", "missing metadata should render blank");
		assert!(
			output.contains("| skipped.example | | | | sweep budget exhausted before dispatch |")
		);

		assert!(output.contains("| bare.example | | | 0ns | missing server metadata |"));
		assert!(output.ends_with("\n4 results in 0ns.\n"));
	}

	#[test]
	fn detail_listing_is_opt_in_and_filters_by_request_result() {
		let outcomes = || {
			vec![
				success(server_name!("good.example"), "alpha"),
				failed(server_name!("bad.example"), Duration::from_secs(1), Fault::Elapsed),
				failed(server_name!("backoff.example"), Duration::ZERO, Fault::Backoff {
					class: Classification::Transient,
					age: Duration::from_secs(30),
					retry: Duration::from_secs(10),
				}),
			]
		};

		let summary = render(outcomes(), Duration::ZERO, ListMode::None, Sort::Origin);

		assert!(!summary.contains("| origin |"));
		assert!(summary.ends_with("\n1 result in 0ns.\n"));

		let successes = render(outcomes(), Duration::ZERO, ListMode::Successes, Sort::Origin);

		assert!(successes.contains("| good.example | alpha |  | 0ns | |"));
		assert!(!successes.contains("bad.example"));
		assert!(!successes.contains("backoff.example"));

		let all = render(outcomes(), Duration::ZERO, ListMode::All, Sort::Origin);

		assert!(all.contains("backoff.example"));

		let errors = render(outcomes(), Duration::ZERO, ListMode::Errors, Sort::Origin);

		assert!(!errors.contains("good.example"));
		assert!(errors.contains("bad.example"));
		assert!(errors.contains("backoff.example"));
		assert!(errors.ends_with("\n1 result in 0ns.\n"));
	}

	#[test]
	fn detail_listing_associates_servers_with_escaped_versions() {
		for list_mode in [ListMode::Successes, ListMode::All] {
			let outcomes = [
				(server_name!("first.example"), "alpha", "1.0"),
				(server_name!("second.example"), "be|ta", "2.0\nrc"),
			]
			.into_iter()
			.map(|(origin, name, version)| {
				let version = Version {
					name: Some(name.to_owned()),
					version: Some(version.to_owned()),
					..Default::default()
				};

				Outcome {
					origin: origin.to_owned(),
					elapsed: Duration::ZERO,
					result: Ok(Some(version)),
				}
			})
			.collect();

			let output = render(outcomes, Duration::ZERO, list_mode, Sort::Origin);

			assert!(output.contains("| first.example | alpha | 1.0 | 0ns | |"));
			assert!(output.contains("| second.example | be\\|ta | 2.0 rc | 0ns | |"));
		}
	}

	#[test]
	fn detail_listing_sorts_by_column_with_origin_as_tie_breaker() {
		let outcomes = || {
			vec![
				Outcome {
					origin: server_name!("slow.example").to_owned(),
					elapsed: Duration::from_secs(2),
					result: Ok(Some(Version::default())),
				},
				failed(server_name!("b-timeout.example"), Duration::from_secs(1), Fault::Elapsed),
				failed(server_name!("a-timeout.example"), Duration::from_secs(1), Fault::Elapsed),
				failed(server_name!("skipped.example"), Duration::ZERO, Fault::NotAttempted),
			]
		};

		let by_origin = render(outcomes(), Duration::ZERO, ListMode::All, Sort::Origin);

		assert_eq!(listed_origins(&by_origin), [
			"a-timeout.example",
			"b-timeout.example",
			"skipped.example",
			"slow.example"
		]);

		let by_elapsed = render(outcomes(), Duration::ZERO, ListMode::All, Sort::Elapsed);

		assert_eq!(listed_origins(&by_elapsed), [
			"skipped.example",
			"a-timeout.example",
			"b-timeout.example",
			"slow.example"
		]);

		let by_fault = render(outcomes(), Duration::ZERO, ListMode::All, Sort::Fault);

		assert_eq!(listed_origins(&by_fault), [
			"slow.example",
			"a-timeout.example",
			"b-timeout.example",
			"skipped.example"
		]);
	}

	fn listed_origins(output: &str) -> Vec<&str> {
		let (_, listing) = output
			.split_once("| origin | name | version | elapsed | fault |\n")
			.expect("detail listing should be rendered");

		listing
			.lines()
			.skip(1)
			.take_while(|line| line.starts_with('|'))
			.filter_map(|line| line.split('|').nth(1))
			.map(str::trim)
			.collect()
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

	fn failed(origin: &ServerName, elapsed: Duration, fault: Fault) -> VersionOutcome {
		Outcome {
			origin: origin.to_owned(),
			elapsed,
			result: Err(fault),
		}
	}
}
