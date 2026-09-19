use std::{
	borrow::Cow,
	cmp::Ordering,
	collections::BTreeMap,
	fmt::{Result as FmtResult, Write as _},
	num::NonZeroUsize,
	time::{Duration, Instant},
};

use clap::ValueEnum;
use futures::{StreamExt, stream::iter};
use ruma::{
	OwnedRoomOrAliasId,
	api::federation::discovery::get_server_version::v1::{Request, Response, Server},
};
use tuwunel_core::{Result, itertools::Itertools, utils::time::now_secs};
use tuwunel_service::federation::feds::Outcome;

use super::{
	ListMode, Sort, SweepArgs, count_results, fault_message, markdown_cell, partition_backoffs,
	prepare, render_totals, sorted, write_cell, write_elapsed_cell,
};
use crate::admin_command;

pub(super) const WIDTH_DEFAULT: NonZeroUsize = NonZeroUsize::new(192).expect("192 is nonzero");
const DEFAULT_FIELDS: &[Field] = &[Field::Name, Field::Version];

#[derive(Default, Eq, Ord, PartialEq, PartialOrd)]
struct Version {
	name: Option<String>,
	version: Option<String>,
	compiler: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, ValueEnum)]
pub(crate) enum Field {
	Name,
	Version,
	Compiler,
}

impl Field {
	fn label(self) -> &'static str {
		match self {
			| Self::Name => "name",
			| Self::Version => "version",
			| Self::Compiler => "compiler",
		}
	}

	fn value(self, version: &Version) -> Option<&str> {
		match self {
			| Self::Name => version.name.as_deref(),
			| Self::Version => version.version.as_deref(),
			| Self::Compiler => version.compiler.as_deref(),
		}
	}
}

#[derive(Clone, Copy, Eq)]
struct VersionClass<'a> {
	fields: &'a [Field],
	version: &'a Version,
}

impl Ord for VersionClass<'_> {
	fn cmp(&self, other: &Self) -> Ordering {
		self.fields.cmp(other.fields).then_with(|| {
			self.fields
				.iter()
				.map(|field| field.value(self.version))
				.cmp(
					other
						.fields
						.iter()
						.map(|field| field.value(other.version)),
				)
		})
	}
}

impl PartialOrd for VersionClass<'_> {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl PartialEq for VersionClass<'_> {
	fn eq(&self, other: &Self) -> bool { self.cmp(other).is_eq() }
}

type ClassCounts<'a> = BTreeMap<VersionClass<'a>, usize>;
type VersionOutcome = Outcome<Option<Version>>;

#[admin_command]
pub(super) async fn feds_version(
	&self,
	room: OwnedRoomOrAliasId,
	fields: Vec<Field>,
	list: bool,
	list_all: bool,
	list_errors: bool,
	sort: Sort,
	sweep: SweepArgs,
) -> Result {
	let prepared = prepare(self, &room, sweep, WIDTH_DEFAULT).await?;
	let backoffs = self.services.federation.peer_backoffs().await;
	let (eligible, outcomes) = partition_backoffs(self, &prepared, &backoffs, now_secs()).await;

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

	let list_mode = ListMode::new(list, list_all, list_errors);
	let fields = selected_fields(fields);
	let output = render(outcomes, total, list_mode, sort, &fields);

	self.write_str(&output).await
}

fn into_version(response: Response) -> Option<Version> {
	response.server.map(|server| {
		let Server { name, version, compiler, .. } = server;

		Version { name, version, compiler }
	})
}

fn selected_fields(mut fields: Vec<Field>) -> Cow<'static, [Field]> {
	if fields.is_empty() {
		return Cow::Borrowed(DEFAULT_FIELDS);
	}

	fields.sort_unstable();
	fields.dedup();
	Cow::Owned(fields)
}

fn render(
	outcomes: Vec<VersionOutcome>,
	total: Duration,
	list_mode: ListMode,
	sort: Sort,
	fields: &[Field],
) -> String {
	let outcomes = sorted(outcomes, sort, fault_cell);
	let mut output = String::new();

	render_into(&mut output, &outcomes, total, list_mode, fields)
		.expect("writing to a String cannot fail");

	output
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
	fields: &[Field],
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
				.entry(VersionClass { fields, version })
				.and_modify(|count| *count = count.saturating_add(1))
				.or_insert(1);

			classes
		});

	write!(output, "| rank | servers |")?;
	for field in fields {
		write!(output, " {} |", field.label())?;
	}

	writeln!(output)?;

	write!(output, "| ---: | ------: |")?;
	for _ in fields {
		write!(output, " :--- |")?;
	}

	writeln!(output)?;

	let classes = counts.into_iter().sorted_unstable_by(
		|(left_class, left_count), (right_class, right_count)| {
			right_count
				.cmp(left_count)
				.then_with(|| left_class.cmp(right_class))
		},
	);

	for (rank, (class, count)) in classes.enumerate() {
		write!(output, "| {} | {count} |", rank.saturating_add(1))?;
		for field in fields {
			write!(output, " {} |", option_cell(field.value(class.version)))?;
		}

		writeln!(output)?;
	}

	if matches!(list_mode, ListMode::None) {
		return render_totals(output, results, total);
	}

	write!(output, "\n| origin | name | version |")?;
	for field in extra_fields(fields) {
		write!(output, " {} |", field.label())?;
	}

	writeln!(output, " elapsed | fault |")?;

	write!(output, "| :--- | :--- | :--- |")?;
	for _ in extra_fields(fields) {
		write!(output, " :--- |")?;
	}

	writeln!(output, " ---: | :--- |")?;
	for outcome in outcomes
		.iter()
		.filter(|outcome| list_mode.includes(outcome))
	{
		render_row(output, outcome, fields)?;
	}

	render_totals(output, results, total)
}

fn extra_fields(fields: &[Field]) -> impl Iterator<Item = Field> + '_ {
	fields
		.iter()
		.copied()
		.filter(|field| !matches!(field, Field::Name | Field::Version))
}

fn render_row(output: &mut String, outcome: &VersionOutcome, fields: &[Field]) -> FmtResult {
	let fault = fault_cell(outcome);
	let version = outcome
		.result
		.as_ref()
		.ok()
		.and_then(Option::as_ref);

	write!(output, "| {} |", outcome.origin)?;
	write_version_cell(output, version, Field::Name)?;
	write_version_cell(output, version, Field::Version)?;

	for field in extra_fields(fields) {
		write_version_cell(output, version, field)?;
	}

	write_elapsed_cell(output, outcome)?;

	let fault = markdown_cell(&fault);
	write_cell(output, &fault)?;
	writeln!(output)
}

fn write_version_cell(output: &mut String, version: Option<&Version>, field: Field) -> FmtResult {
	match version {
		| Some(version) => write!(output, " {} |", option_cell(field.value(version))),
		| None => write!(output, " |"),
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
	use tuwunel_service::federation::{Classification, feds::Fault};

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

		let output =
			render(outcomes, Duration::ZERO, ListMode::All, Sort::Origin, DEFAULT_FIELDS);

		let popular = output
			.find("| 1 | 2 | zeta |  |")
			.expect("popular version should be rendered first");

		let rare = output
			.find("| 2 | 1 | alpha |  |")
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
	fn selected_fields_control_summary_grouping_and_detail_columns() {
		let outcomes = || {
			let outcome = |origin: &ServerName, compiler: &str| Outcome {
				origin: origin.to_owned(),
				elapsed: Duration::ZERO,
				result: Ok(Some(Version {
					name: Some("tuwunel".to_owned()),
					version: Some("1.2".to_owned()),
					compiler: Some(compiler.to_owned()),
				})),
			};

			vec![
				outcome(server_name!("first.example"), "rustc-a"),
				outcome(server_name!("second.example"), "rustc-b"),
			]
		};

		let default =
			render(outcomes(), Duration::ZERO, ListMode::None, Sort::Origin, DEFAULT_FIELDS);

		assert!(default.contains("| rank | servers | name | version |\n"));
		assert!(default.contains("| 1 | 2 | tuwunel | 1.2 |\n"));
		assert!(!default.contains("compiler"));

		let fields = [Field::Name, Field::Version, Field::Compiler];

		let selected = render(outcomes(), Duration::ZERO, ListMode::All, Sort::Origin, &fields);

		assert!(selected.contains("| rank | servers | name | version | compiler |\n"));

		assert!(selected.contains("| 1 | 1 | tuwunel | 1.2 | rustc-a |\n"));
		assert!(selected.contains("| 2 | 1 | tuwunel | 1.2 | rustc-b |\n"));
		assert!(selected.contains("| origin | name | version | compiler | elapsed | fault |\n"));
		assert!(selected.contains("| first.example | tuwunel | 1.2 | rustc-a | 0ns | |\n"));
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

		let summary =
			render(outcomes(), Duration::ZERO, ListMode::None, Sort::Origin, DEFAULT_FIELDS);

		assert!(!summary.contains("| origin |"));
		assert!(summary.ends_with("\n1 result in 0ns.\n"));

		let successes =
			render(outcomes(), Duration::ZERO, ListMode::Successes, Sort::Origin, DEFAULT_FIELDS);

		assert!(successes.contains("| good.example | alpha |  | 0ns | |"));
		assert!(!successes.contains("bad.example"));
		assert!(!successes.contains("backoff.example"));

		let all = render(outcomes(), Duration::ZERO, ListMode::All, Sort::Origin, DEFAULT_FIELDS);

		assert!(all.contains("backoff.example"));

		let errors =
			render(outcomes(), Duration::ZERO, ListMode::Errors, Sort::Origin, DEFAULT_FIELDS);

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

			let output =
				render(outcomes, Duration::ZERO, list_mode, Sort::Origin, DEFAULT_FIELDS);

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

		let by_origin =
			render(outcomes(), Duration::ZERO, ListMode::All, Sort::Origin, DEFAULT_FIELDS);

		assert_eq!(listed_origins(&by_origin), [
			"a-timeout.example",
			"b-timeout.example",
			"skipped.example",
			"slow.example"
		]);

		let by_elapsed =
			render(outcomes(), Duration::ZERO, ListMode::All, Sort::Elapsed, DEFAULT_FIELDS);

		assert_eq!(listed_origins(&by_elapsed), [
			"skipped.example",
			"a-timeout.example",
			"b-timeout.example",
			"slow.example"
		]);

		let by_fault =
			render(outcomes(), Duration::ZERO, ListMode::All, Sort::Fault, DEFAULT_FIELDS);

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
