//! The background daemon: the file logger, the single-instance lock, transcript listing, the
//! ingest pass, the collector loop and `daemon run`.

pub mod collector;
pub mod ingest_pass;
pub mod lock;
pub mod log;
pub mod run;
pub mod transcripts;
