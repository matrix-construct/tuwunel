use ruma::events::relation::RelationType;
use serde::{Deserialize, de::IgnoredAny};
use serde_json::{from_str, value::RawValue as RawJsonValue};

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

#[derive(Deserialize)]
struct ChildContent<'a> {
	#[serde(rename = "m.relates_to", borrow)]
	relates_to: Option<&'a RawJsonValue>,
}

#[derive(Deserialize)]
struct ChildRelation {
	rel_type: Option<IgnoredAny>,
}

impl<E: Event> RelationTypeEqual<E> for RelationType {
	fn relation_type_equal(&self, event: &E) -> bool {
		event
			.get_content()
			.map(|c: ExtractRelatesToEventId| c.relates_to.rel_type)
			.is_ok_and(|r| r == *self)
	}
}

/// Whether the event's content names a relation type in its `m.relates_to`.
///
/// Any non-null `rel_type` counts unread, so one the typed relations cannot
/// parse still counts, while a plain reply names none. Content that does not
/// decode, or whose `m.relates_to` is neither null nor an object, counts too, so
/// an unreadable relation never passes for none.
pub(super) fn has_rel_type<E: Event>(event: &E) -> bool {
	from_str(event.content().get())
		.map_or(true, |content: ChildContent<'_>| content.relates_to.is_some_and(names_rel_type))
}

/// Whether a present `m.relates_to` names a relation type.
///
/// Only an object is read for its fields, since a derived struct also takes an
/// array's items by position; anything else counts as naming one.
fn names_rel_type(relation: &RawJsonValue) -> bool {
	let relation = relation.get();

	!relation.starts_with('{')
		|| from_str(relation).map_or(true, |relation: ChildRelation| relation.rel_type.is_some())
}
