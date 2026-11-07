//! Short event and state key ids, and the residue their damage leaves.
//!
//! The injectivity repair's final verification folds the same census into the
//! outcome it records, so a run reports what verification would record for the
//! database as it stands, each defect once. It compares columns, so it runs only
//! where nothing else writes.

use super::{Check, tally};
use crate::migrations::injectivity::check;

pub(super) static CHECK: Check = Check {
	name: "short-ids",
	about: "Short event and state key ids pair one to one, every state resolves cleanly onto \
	        sound ones, and no unreachable state or unowned short room id remains",
	quiescent: true,
	walk: |services, limit| tally(check(services), limit),
};
