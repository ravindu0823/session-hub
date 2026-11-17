//! The SQLite index `hub.db`: port of `src/store/schema.ts` and `src/store/db.ts`. The TS and Rust
//! processes read and write the same file, so schema, pragmas, queries and stored JSON match TS.

mod db;
mod schema;

pub use db::{
    ANSWERS_SQL, LatestReply, LiveStatus, MessageRow, Role, SearchHit, SendRow, SendState,
    SessionList, SessionRow, SessionView, Store, StoreError, StoredStatus, ViewWaitingItem,
    iso_utc_ms,
};
pub use schema::MIGRATIONS;
