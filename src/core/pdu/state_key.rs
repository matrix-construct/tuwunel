use smallstr::SmallString;

use super::ShortId;

pub type StateKey = SmallString<[u8; INLINE_SIZE]>;
pub type ShortStateKey = ShortId;

const INLINE_SIZE: usize = 48;
