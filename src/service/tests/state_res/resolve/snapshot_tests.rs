//! Snapshot tests.

// Test the minimal set of events required to create a room with the
// "private_chat" preset.
snapshot_test!(minimal_private_chat, ["bootstrap-private-chat.json"]);

// Start with a private room, then transition its join rules to restricted, then
// to public. The events in the second file are tied topologically, so they must
// have the tiebreaking algorithm applied. The ordering should be decided by
// the `origin_server_ts` fields of these events, not the `event_id` fields. The
// power levels of these events are equivalent, so they don't really matter.
snapshot_test!(origin_server_ts_tiebreak, [
	"bootstrap-private-chat.json",
	"origin-server-ts-tiebreak.json"
],);

// Test that state res v2.0 is implemented starting from the unconflicted set,
// and NOT the empty set, leading to there being no join rules state.
//
// This example comes directly from the "Problem A" section of MSC4297.
snapshot_test_contrived_states!(
	msc4297_problem_a_state_res_v2_0,
	["MSC4297-problem-A/pdus-v11.json"],
	["MSC4297-problem-A/state-bob.json", "MSC4297-problem-A/state-charlie.json"]
);

// Test that state res v2.1 is implemented starting from the empty set, and NOT
// the unconflicted set.
//
// This example comes directly from the "Problem A" section of MSC4297.
snapshot_test_contrived_states!(
	msc4297_problem_a_state_res_v2_1,
	["MSC4297-problem-A/pdus-hydra.json"],
	["MSC4297-problem-A/state-bob.json", "MSC4297-problem-A/state-charlie.json"]
);

// Test that state res v2.0 does NOT consider the conflicted state subgraph as
// part of the full conflicted state set, leading to the state resetting to the
// first power levels event.
//
// This example comes directly from the "Problem B" section of MSC4297.
snapshot_test_contrived_states!(
	msc4297_problem_b_state_res_v2_0,
	["MSC4297-problem-B/pdus-v11.json"],
	["MSC4297-problem-B/state-eve.json", "MSC4297-problem-B/state-zara.json"]
);

// Test that state res v2.1 considers the conflicted state subgraph as part of
// the full conflicted state set.
//
// This example comes directly from the "Problem B" section of MSC4297.
snapshot_test_contrived_states!(
	msc4297_problem_b_state_res_v2_1,
	["MSC4297-problem-B/pdus-hydra.json"],
	["MSC4297-problem-B/state-eve.json", "MSC4297-problem-B/state-zara.json"]
);
