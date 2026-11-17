//! The live session list (`claude agents --json`) and the sessions folder watch.

pub mod agents;
pub mod watch;

pub use agents::{
    AgentsError, ClaudeCli, LiveSession, MAX_AGENTS_STDOUT_BYTES, RealClaudeCli, StopToken,
    find_claude, is_own_relay, parse_agents, parse_status,
};
pub use watch::{WatchError, WatchHandle, should_fire, watch_sessions, watch_sessions_with};
