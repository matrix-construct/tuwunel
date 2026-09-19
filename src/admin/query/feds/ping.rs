use std::{
	borrow::Cow,
	fmt::{Display, Formatter, Result as FmtResult, Write as _},
	iter::once,
	num::NonZeroUsize,
	time::{Duration, Instant},
};

use futures::{StreamExt, stream::iter};
use ruma::{OwnedRoomOrAliasId, api::federation::discovery::get_server_version::v1::Request};
use tuwunel_core::{
	Result, implement,
	itertools::Itertools,
	utils::time::{Elapsed, now_secs},
};
use tuwunel_service::federation::{
	PeerBackoff,
	feds::{Fault, Outcome},
};

use super::{
	Backoffs, ListMode, Sort, SweepArgs, count_results, fault_message, markdown_cell,
	partition_backoffs, prepare, render_totals, retry_after, sorted, write_cell,
	write_elapsed_cell,
};
use crate::admin_command;

pub(super) const WIDTH_DEFAULT: NonZeroUsize = NonZeroUsize::new(192).expect("192 is nonzero");

const OPEN_BUCKET: Duration = Duration::from_secs(5);

/// Exclusive upper bounds of the latency histogram buckets.
///
/// The last bound opens the final bucket, which collects every slower
/// response.
const BUCKETS: &[Duration] = &[
	Duration::from_millis(10),
	Duration::from_millis(25),
	Duration::from_millis(50),
	Duration::from_millis(100),
	Duration::from_millis(250),
	Duration::from_millis(500),
	Duration::from_secs(1),
	Duration::from_millis(2_500),
	OPEN_BUCKET,
];

type PingOutcome = Outcome<()>;

/// Settlement class of one destination.
///
/// The classes partition every outcome, so their counts sum to the
/// destination count.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Disposition {
	Responded,
	Failed,
	TimedOut,
	BackingOff,
	NotAttempted,
}

impl Disposition {
	const ALL: [Self; 5] = [
		Self::Responded,
		Self::Failed,
		Self::TimedOut,
		Self::BackingOff,
		Self::NotAttempted,
	];
}

/// Distribution of one latency population.
///
/// Percentiles use the nearest rank and the deviation is the population
/// standard deviation, both computed in integer nanoseconds.
struct Stats {
	count: usize,
	min: Duration,
	p50: Duration,
	mean: Duration,
	p90: Duration,
	p99: Duration,
	max: Duration,
	stddev: Duration,
}

/// Fraction of a population.
///
/// Renders as a percentage truncated to one decimal place.
struct Share {
	count: usize,
	total: usize,
}

#[admin_command]
pub(super) async fn feds_ping(
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
	let now = now_secs();
	let (eligible, outcomes) = partition_backoffs(self, &prepared, &backoffs, now).await;

	let started = Instant::now();
	let responses = self
		.services
		.federation
		.fanout_to(iter(eligible), |_| Request::new(), prepared.opts)
		.map(|outcome| Outcome {
			origin: outcome.origin,
			elapsed: outcome.elapsed,
			result: outcome.result.map(drop),
		});

	let outcomes: Vec<_> = iter(outcomes).chain(responses).collect().await;
	let total = started.elapsed();
	let list_mode = ListMode::new(list, list_all, list_errors);
	let outcomes = sorted(outcomes, sort, fault_cell);
	let output = render(&outcomes, &backoffs, now, total, list_mode);

	self.write_str(&output).await
}

fn fault_cell(outcome: &PingOutcome) -> Cow<'static, str> {
	outcome
		.result
		.as_ref()
		.err()
		.map(fault_message)
		.unwrap_or_default()
}

fn render(
	outcomes: &[PingOutcome],
	backoffs: &Backoffs,
	now: u64,
	total: Duration,
	list_mode: ListMode,
) -> String {
	let mut output = String::new();

	render_into(&mut output, outcomes, backoffs, now, total, list_mode)
		.expect("writing to a String cannot fail");

	output
}

fn render_into(
	output: &mut String,
	outcomes: &[PingOutcome],
	backoffs: &Backoffs,
	now: u64,
	total: Duration,
	list_mode: ListMode,
) -> FmtResult {
	render_dispositions(output, outcomes)?;

	let responded = sorted_elapsed(outcomes, |disposition| disposition == Disposition::Responded);
	let failed = sorted_elapsed(outcomes, |disposition| disposition == Disposition::Failed);
	let attempted = sorted_elapsed(outcomes, Disposition::attempted);

	render_latencies(output, &responded, &failed, &attempted)?;
	render_histogram(output, &responded)?;

	if !matches!(list_mode, ListMode::None) {
		render_listing(output, outcomes, backoffs, now, list_mode)?;
	}

	render_totals(output, count_results(outcomes), total)
}

fn render_dispositions(output: &mut String, outcomes: &[PingOutcome]) -> FmtResult {
	writeln!(output, "| disposition | servers | share |")?;
	writeln!(output, "| :--- | ---: | ---: |")?;

	let total = outcomes.len();
	for disposition in Disposition::ALL {
		let count = outcomes
			.iter()
			.filter(|outcome| Disposition::of(outcome) == disposition)
			.count();

		writeln!(output, "| {} | {count} | {} |", disposition.label(), Share { count, total })?;
	}

	Ok(())
}

#[implement(Disposition)]
fn of(outcome: &PingOutcome) -> Self {
	match &outcome.result {
		| Ok(()) => Self::Responded,
		| Err(Fault::Error(_)) => Self::Failed,
		| Err(Fault::Elapsed) => Self::TimedOut,
		| Err(Fault::Backoff { .. }) => Self::BackingOff,
		| Err(Fault::NotAttempted) => Self::NotAttempted,
	}
}

#[implement(Disposition)]
fn label(self) -> &'static str {
	match self {
		| Self::Responded => "responded",
		| Self::Failed => "failed",
		| Self::TimedOut => "timed out",
		| Self::BackingOff => "backing off",
		| Self::NotAttempted => "not attempted",
	}
}

impl Display for Share {
	fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
		let permille = self
			.count
			.saturating_mul(1000)
			.checked_div(self.total)
			.unwrap_or_default();

		write!(formatter, "{}.{}%", permille / 10, permille % 10)
	}
}

/// Collects the ascending elapsed times of the selected dispositions.
///
/// The statistics and histogram index the population, which forces the
/// buffer.
fn sorted_elapsed(
	outcomes: &[PingOutcome],
	select: impl Fn(Disposition) -> bool,
) -> Vec<Duration> {
	outcomes
		.iter()
		.filter(|outcome| select(Disposition::of(outcome)))
		.map(|outcome| outcome.elapsed)
		.sorted_unstable()
		.collect()
}

/// Whether the destination's elapsed time measures a real request.
#[implement(Disposition)]
fn attempted(self) -> bool { !matches!(self, Self::BackingOff | Self::NotAttempted) }

fn render_latencies(
	output: &mut String,
	responded: &[Duration],
	failed: &[Duration],
	attempted: &[Duration],
) -> FmtResult {
	let populations = [
		("responded", Stats::new(responded)),
		("failed", Stats::new(failed)),
		("attempted", Stats::new(attempted)),
	];

	if populations
		.iter()
		.all(|(_, stats)| stats.is_none())
	{
		return Ok(());
	}

	writeln!(output, "\n| population | count | min | p50 | mean | p90 | p99 | max | stddev |")?;
	writeln!(output, "| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")?;

	for (label, stats) in populations
		.iter()
		.filter_map(|(label, stats)| stats.as_ref().map(|stats| (label, stats)))
	{
		writeln!(
			output,
			"| {label} | {} | {} | {} | {} | {} | {} | {} | {} |",
			stats.count,
			Elapsed::from(stats.min),
			Elapsed::from(stats.p50),
			Elapsed::from(stats.mean),
			Elapsed::from(stats.p90),
			Elapsed::from(stats.p99),
			Elapsed::from(stats.max),
			Elapsed::from(stats.stddev),
		)?;
	}

	Ok(())
}

impl Stats {
	/// Summarizes an ascending population, `None` when it is empty.
	///
	/// Callers sort first because the percentiles index the slice by rank.
	fn new(sorted: &[Duration]) -> Option<Self> {
		let (&min, &max) = sorted.first().zip(sorted.last())?;
		let count = sorted.len();
		let divisor = u128::try_from(count).unwrap_or(u128::MAX);
		let mean = sorted
			.iter()
			.map(Duration::as_nanos)
			.sum::<u128>()
			.checked_div(divisor)
			.unwrap_or_default();

		let variance = sorted
			.iter()
			.map(Duration::as_nanos)
			.map(|elapsed| elapsed.abs_diff(mean))
			.map(|deviation| deviation.saturating_mul(deviation))
			.sum::<u128>()
			.checked_div(divisor)
			.unwrap_or_default();

		Some(Self {
			count,
			min,
			p50: percentile(sorted, 50),
			mean: from_nanos(mean),
			p90: percentile(sorted, 90),
			p99: percentile(sorted, 99),
			max,
			stddev: from_nanos(variance.isqrt()),
		})
	}
}

/// Selects the nearest-rank percentile of an ascending population.
///
/// The rank is the ceiling of `percent` times the population size; an empty
/// population yields zero.
fn percentile(sorted: &[Duration], percent: usize) -> Duration {
	let index = sorted
		.len()
		.saturating_mul(percent)
		.div_ceil(100)
		.saturating_sub(1);

	sorted.get(index).copied().unwrap_or_default()
}

fn from_nanos(nanos: u128) -> Duration {
	Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

fn render_histogram(output: &mut String, responded: &[Duration]) -> FmtResult {
	if responded.is_empty() {
		return Ok(());
	}

	writeln!(output, "\n| latency | servers | cumulative |")?;
	writeln!(output, "| :--- | ---: | ---: |")?;

	let total = responded.len();
	let cumulative = BUCKETS
		.iter()
		.map(|bound| responded.partition_point(|elapsed| elapsed < bound))
		.chain(once(total));

	let labels = BUCKETS
		.iter()
		.map(|bound| ("<", *bound))
		.chain(once((">=", OPEN_BUCKET)));

	for ((previous, count), (relation, bound)) in once(0)
		.chain(cumulative)
		.tuple_windows()
		.zip(labels)
	{
		let servers = count.saturating_sub(previous);

		writeln!(output, "| {relation} {} | {servers} | {} |", Elapsed::from(bound), Share {
			count,
			total
		})?;
	}

	Ok(())
}

fn render_listing(
	output: &mut String,
	outcomes: &[PingOutcome],
	backoffs: &Backoffs,
	now: u64,
	list_mode: ListMode,
) -> FmtResult {
	writeln!(output, "\n| origin | elapsed | class | newest | oldest | retry | fault |")?;
	writeln!(output, "| :--- | ---: | :--- | ---: | ---: | ---: | :--- |")?;

	for outcome in outcomes
		.iter()
		.filter(|outcome| list_mode.includes(outcome))
	{
		render_row(output, outcome, backoffs.get(&outcome.origin), now)?;
	}

	Ok(())
}

fn render_row(
	output: &mut String,
	outcome: &PingOutcome,
	backoff: Option<&PeerBackoff>,
	now: u64,
) -> FmtResult {
	write!(output, "| {} |", outcome.origin)?;
	write_elapsed_cell(output, outcome)?;

	match backoff {
		| None => write!(output, " | | | |")?,
		| Some(backoff) => write_backoff_cells(output, backoff, now)?,
	}

	let fault = fault_cell(outcome);
	let fault = markdown_cell(&fault);

	write_cell(output, &fault)?;
	writeln!(output)
}

fn write_backoff_cells(output: &mut String, backoff: &PeerBackoff, now: u64) -> FmtResult {
	let newest = Duration::from_secs(now.saturating_sub(backoff.anchor_secs));
	let oldest = Duration::from_secs(now.saturating_sub(backoff.oldest_secs));

	write!(
		output,
		" {:?} | {} | {} |",
		backoff.class,
		Elapsed::from(newest),
		Elapsed::from(oldest)
	)?;

	match retry_after(backoff, now) {
		| None => write!(output, " |"),
		| Some(retry) => write!(output, " {} |", Elapsed::from(retry)),
	}
}

#[cfg(test)]
mod tests {
	use ruma::{ServerName, server_name};
	use tuwunel_core::{Error, err};
	use tuwunel_service::federation::Classification;

	use super::*;

	#[test]
	fn summary_reports_dispositions_latency_statistics_and_histogram() {
		let outcomes = vec![
			responded(server_name!("fast.example"), 10),
			responded(server_name!("medium.example"), 20),
			responded(server_name!("slow.example"), 60),
			failed(server_name!("broken.example"), 40, Fault::Error(error())),
			failed(server_name!("late.example"), 1_000, Fault::Elapsed),
			failed(server_name!("skipped.example"), 0, Fault::NotAttempted),
		];

		let output =
			rendered(outcomes, &Backoffs::new(), 0, Duration::ZERO, ListMode::None, Sort::Origin);

		assert!(output.starts_with("| disposition | servers | share |\n"));
		assert!(output.contains("| responded | 3 | 50.0% |\n"));
		assert!(output.contains("| failed | 1 | 16.6% |\n"));
		assert!(output.contains("| timed out | 1 | 16.6% |\n"));
		assert!(output.contains("| backing off | 0 | 0.0% |\n"));
		assert!(output.contains("| not attempted | 1 | 16.6% |\n"));

		let header = "| population | count | min | p50 | mean | p90 | p99 | max | stddev |\n";
		let responded = "| responded | 3 | 10ms | 20ms | 30ms | 60ms | 60ms | 60ms | 21.6ms |\n";
		let failed = "| failed | 1 | 40ms | 40ms | 40ms | 40ms | 40ms | 40ms | 0ns |\n";

		assert!(output.contains(header));
		assert!(output.contains(responded));
		assert!(output.contains(failed));
		assert!(output.contains("| attempted | 5 | 10ms | 40ms | 226ms |"));

		assert!(output.contains("| latency | servers | cumulative |\n"));
		assert!(output.contains("| < 10ms | 0 | 0.0% |\n"));
		assert!(output.contains("| < 25ms | 2 | 66.6% |\n"));
		assert!(output.contains("| < 100ms | 1 | 100.0% |\n"));
		assert!(output.contains("| >= 5s | 0 | 100.0% |\n"));
		assert!(!output.contains("| origin |"));
		assert!(output.ends_with("\n3 results in 0ns.\n"));
	}

	#[test]
	fn empty_populations_render_no_latency_tables() {
		let outcomes = vec![failed(server_name!("skipped.example"), 0, Fault::NotAttempted)];

		let output =
			rendered(outcomes, &Backoffs::new(), 0, Duration::ZERO, ListMode::None, Sort::Origin);

		assert!(output.contains("| not attempted | 1 | 100.0% |\n"));
		assert!(!output.contains("| population |"));
		assert!(!output.contains("| latency |"));
		assert!(output.ends_with("\n0 results in 0ns.\n"));
	}

	#[test]
	fn listing_reports_peer_status_beside_each_origin() {
		let now = 1_000;
		let backoffs = Backoffs::from([
			(server_name!("recovered.example").to_owned(), PeerBackoff {
				class: Classification::Transient,
				anchor_secs: 900,
				oldest_secs: 600,
				delay_secs: 60,
			}),
			(server_name!("held.example").to_owned(), PeerBackoff {
				class: Classification::Permanent,
				anchor_secs: 990,
				oldest_secs: 990,
				delay_secs: 3_600,
			}),
		]);

		let outcomes = || {
			vec![
				responded(server_name!("clean.example"), 10),
				responded(server_name!("recovered.example"), 20),
				failed(server_name!("held.example"), 0, Fault::Backoff {
					class: Classification::Permanent,
					age: Duration::from_secs(10),
					retry: Duration::from_secs(3_590),
				}),
			]
		};

		let all =
			rendered(outcomes(), &backoffs, now, Duration::ZERO, ListMode::All, Sort::Origin);

		assert!(all.contains("| origin | elapsed | class | newest | oldest | retry | fault |\n"));
		assert!(all.contains("| clean.example | 10ms | | | | | |\n"));
		assert!(all.contains("| recovered.example | 20ms | Transient | 100s | 400s | | |\n"));
		assert!(all.contains(
			"| held.example | | Permanent | 10s | 10s | 3590s | peer backoff (Permanent, age \
			 10s, retry 3590s) |\n"
		));

		let successes = rendered(
			outcomes(),
			&backoffs,
			now,
			Duration::ZERO,
			ListMode::Successes,
			Sort::Origin,
		);

		assert!(successes.contains("clean.example"));
		assert!(!successes.contains("held.example"));

		let errors =
			rendered(outcomes(), &backoffs, now, Duration::ZERO, ListMode::Errors, Sort::Origin);

		assert!(!errors.contains("clean.example"));
		assert!(errors.contains("held.example"));
	}

	#[test]
	fn listing_sorts_by_elapsed_with_origin_as_tie_breaker() {
		let outcomes = vec![
			responded(server_name!("slow.example"), 50),
			responded(server_name!("b.example"), 10),
			responded(server_name!("a.example"), 10),
		];

		let output =
			rendered(outcomes, &Backoffs::new(), 0, Duration::ZERO, ListMode::All, Sort::Elapsed);

		let (_, listing) = output
			.split_once("| retry | fault |\n")
			.expect("detail listing should be rendered");

		let origins: Vec<_> = listing
			.lines()
			.skip(1)
			.take_while(|line| line.starts_with('|'))
			.filter_map(|line| line.split('|').nth(1))
			.map(str::trim)
			.collect();

		assert_eq!(origins, ["a.example", "b.example", "slow.example"]);
	}

	#[test]
	fn percentiles_use_nearest_rank() {
		let sorted: Vec<_> = (1..=10).map(Duration::from_millis).collect();

		assert_eq!(percentile(&sorted, 0), Duration::from_millis(1));
		assert_eq!(percentile(&sorted, 50), Duration::from_millis(5));
		assert_eq!(percentile(&sorted, 90), Duration::from_millis(9));
		assert_eq!(percentile(&sorted, 99), Duration::from_millis(10));
		assert_eq!(percentile(&[], 50), Duration::ZERO);
	}

	fn rendered(
		outcomes: Vec<PingOutcome>,
		backoffs: &Backoffs,
		now: u64,
		total: Duration,
		list_mode: ListMode,
		sort: Sort,
	) -> String {
		let outcomes = sorted(outcomes, sort, fault_cell);

		render(&outcomes, backoffs, now, total, list_mode)
	}

	fn responded(origin: &ServerName, elapsed_ms: u64) -> PingOutcome {
		Outcome {
			origin: origin.to_owned(),
			elapsed: Duration::from_millis(elapsed_ms),
			result: Ok(()),
		}
	}

	fn failed(origin: &ServerName, elapsed_ms: u64, fault: Fault) -> PingOutcome {
		Outcome {
			origin: origin.to_owned(),
			elapsed: Duration::from_millis(elapsed_ms),
			result: Err(fault),
		}
	}

	fn error() -> Error { err!(Request(Unknown("connection refused"))) }
}
