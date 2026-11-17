//! Status extraction and answered items: ports of `src/status/extract.ts` and `src/status/answered.ts`.

mod answered;
mod extract;

pub use answered::answered_ids;
pub use extract::{Status, WaitingItem, extract_status};
