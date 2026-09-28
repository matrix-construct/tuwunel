use super::{BackoffCounters, BackoffMetrics, Context, Suppression, Verdicts};

#[test]
fn each_verdict_counts_in_its_own_cell() {
	let counters = BackoffCounters::default();
	let lookups = [
		(Context::Fetch, Suppression::Absent),
		(Context::Auth, Suppression::Allow),
		(Context::Upgrade, Suppression::Deny),
		(Context::Upgrade, Suppression::Absent),
		(Context::Incoming, Suppression::Deny),
		(Context::Incoming, Suppression::Deny),
	];

	for (ctx, verdict) in &lookups {
		counters.count(*ctx, verdict);
	}

	let expected = BackoffMetrics {
		fetch: Verdicts { absent: 1, allowed: 0, denied: 0 },
		auth: Verdicts { absent: 0, allowed: 1, denied: 0 },
		upgrade: Verdicts { absent: 1, allowed: 0, denied: 1 },
		incoming: Verdicts { absent: 0, allowed: 0, denied: 2 },
	};

	assert_eq!(counters.snapshot(), expected);
}
