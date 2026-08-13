use ruma::events::relation::RelationType;
use serde::Deserialize;

use super::Event;

/// Compares an event's relation type with a requested relation type.
///
/// Implementations inspect the `m.relates_to.rel_type` field in event content.
/// Missing or malformed relation content does not match.
pub trait RelationTypeEqual<E: Event> {
	/// Returns whether the event declares this relation type.
	///
	/// The comparison deserializes only the relation fields needed for the
	/// check. Content that cannot be deserialized returns false.
	fn relation_type_equal(&self, event: &E) -> bool;
}

#[derive(Clone, Debug, Deserialize)]
struct ExtractRelatesToEventId {
	#[serde(rename = "m.relates_to")]
	relates_to: ExtractRelType,
}

#[derive(Clone, Debug, Deserialize)]
struct ExtractRelType {
	rel_type: RelationType,
}

impl<E: Event> RelationTypeEqual<E> for RelationType {
	fn relation_type_equal(&self, event: &E) -> bool {
		event
			.get_content()
			.map(|c: ExtractRelatesToEventId| c.relates_to.rel_type)
			.is_ok_and(|r| r == *self)
	}
}
