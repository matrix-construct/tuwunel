use futures::TryStreamExt;
use tuwunel_core::{Result, err, implement, itertools::Itertools, utils::IterStream};
use tuwunel_service::integrity::{Check, Summary, checks, find};

use crate::{Context, admin_command};

#[admin_command]
pub(super) async fn integrity_check(&self, names: Vec<String>, limit: usize) -> Result {
	if !names.is_empty() {
		names.iter().try_for_each(|name| {
			find(name)
				.map(|_| ())
				.ok_or_else(|| err!("no integrity check is named {name}"))
		})?;

		return self
			.run_checks(names.iter().filter_map(|name| find(name)), limit)
			.await;
	}

	let runnable = checks().filter(|check| check.runnable(self.services));

	self.run_checks(runnable, limit).await?;

	let skipped = checks()
		.filter(|check| !check.runnable(self.services))
		.map(|check| check.name);

	if skipped.clone().next().is_none() {
		return Ok(());
	}

	self.write_string(format!("\nSkipped on a writable database: {}\n", skipped.format(", ")))
		.await
}

#[implement(Context, params = "<'_>")]
async fn run_checks<Checks>(&self, checks: Checks, limit: usize) -> Result
where
	Checks: Iterator<Item = &'static Check> + Send,
{
	checks
		.try_stream()
		.try_for_each(async |check| self.report(check, limit).await)
		.await
}

/// Writes one check's result, leaving a failed check for the next to follow.
///
/// A stop request is the exception, which ends the whole run.
#[implement(Context, params = "<'_>")]
async fn report(&self, check: &Check, limit: usize) -> Result {
	match check.run(self.services, limit).await {
		| Err(error) if error.is_interrupted() => Err(error),
		| Err(error) => write!(self, "{}: failed: {error}\n", check.name).await,
		| Ok(summary) => self.write_string(rendered(check, &summary)).await,
	}
}

fn rendered(check: &Check, summary: &Summary) -> String {
	match summary.findings() {
		| 0 => format!("{}: clean\n", check.name),
		| findings => {
			let counts = summary
				.counts
				.iter()
				.format_with(", ", |(kind, count), entry| {
					entry(&format_args!("{kind}: {count}"))
				});

			let samples = summary
				.samples
				.iter()
				.format_with("", |sample, entry| entry(&format_args!("- {sample}\n")));

			format!("{}: {findings} found ({counts})\n{samples}", check.name)
		},
	}
}
