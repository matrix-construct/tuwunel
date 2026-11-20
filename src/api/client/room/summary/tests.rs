use ruma::OwnedServerName;

use super::hierarchy_sweep;

#[test]
fn summary_destinations_are_distinct_and_bounded() {
	let mut servers: Vec<_> = (0..10)
		.map(|i| OwnedServerName::try_from(format!("s{i}.example")).expect("valid server"))
		.collect();
	servers.insert(1, servers[0].clone());
	let (selected, _) = hierarchy_sweep(&servers);
	let selected: Vec<_> = selected.collect();
	assert_eq!(selected.len(), 8, "a summary asks at most eight distinct servers");
	assert_eq!(selected[0], servers[0], "retain the first destination");
	assert_eq!(selected[1..], servers[2..9], "a duplicate does not spend a destination");
}

#[test]
fn federation_lookup_response_limits() {
	let (_, opts) = hierarchy_sweep(&[]);
	assert_eq!(opts.limit, Some(8 * 1024 * 1024), "hierarchies include child summaries");
}
