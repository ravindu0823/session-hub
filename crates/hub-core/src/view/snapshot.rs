//! Read-only snapshot of everything the TUI and the bridge show outside History and Send
//! (`src/view/snapshot.ts`). The serialized form is what the tray consumes.
use serde::{Deserialize, Serialize};

use crate::config::STRAY_WARN_MS;
use crate::security::clean_text;
use crate::store::{LiveStatus, SessionView, Store, StoreError, StoredStatus, ViewWaitingItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonState {
    Running,
    Stopped,
}

/// A profile whose live list could not be refreshed, with the time it went stale (epoch ms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleProfile {
    pub profile: String,
    pub since: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMeta {
    pub unknown_count: i64,
    pub unknown_version: Option<String>,
    pub stale: Vec<StaleProfile>,
    pub daemon: DaemonState,
    /// Relay messages that landed in a session other than the one they were sent to, while the
    /// last one is under `STRAY_WARN_MS` old; else 0.
    pub relay_stray_count: i64,
    /// Session rows skipped because they could not be decoded (`Store::list_sessions_counted`).
    /// Not part of the TS snapshot: left out of the JSON while 0, so the shape stays the same.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unreadable_sessions: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub waiting_items: i64,
    pub needs_approval: i64,
    pub live: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubSnapshot {
    pub sessions: Vec<SessionView>,
    pub meta: SnapshotMeta,
    pub totals: Totals,
}

fn clean_status(st: StoredStatus) -> StoredStatus {
    StoredStatus {
        phase: st.phase.map(|p| clean_text(&p)),
        remaining: st.remaining.iter().map(|r| clean_text(r)).collect(),
        waiting: st
            .waiting
            .into_iter()
            .map(|w| {
                let mut item = w.item;
                item.id = item.id.map(|i| clean_text(&i));
                item.text = clean_text(&item.text);
                item.children = item.children.iter().map(|c| clean_text(c)).collect();
                ViewWaitingItem { item, ..w }
            })
            .collect(),
        recommendations: st.recommendations.iter().map(|r| clean_text(r)).collect(),
        ..st
    }
}

/// Every string the panes print from a session goes through `clean_text` here, once per read.
pub fn clean_session(s: SessionView) -> SessionView {
    SessionView {
        name: clean_text(&s.name),
        cwd: clean_text(&s.cwd),
        profile: clean_text(&s.profile),
        status: s.status.map(clean_status),
        ..s
    }
}

/// JS `Number(s)` for the shapes meta values take: `None` when the result is NaN or infinite.
fn js_finite_number(s: &str) -> Option<f64> {
    let t = s.trim_matches(crate::text::is_js_space);
    if t.is_empty() {
        return Some(0.0);
    }
    let radix = |p: &[&str], r: u32| -> Option<Option<f64>> {
        let rest = p.iter().find_map(|p| t.strip_prefix(p))?;
        Some(
            (!rest.is_empty())
                .then(|| u64::from_str_radix(rest, r).ok().map(|n| n as f64))
                .flatten(),
        )
    };
    let v = radix(&["0x", "0X"], 16)
        .or_else(|| radix(&["0o", "0O"], 8))
        .or_else(|| radix(&["0b", "0B"], 2))
        .unwrap_or_else(|| {
            // Rust also accepts "inf" and "nan"; both are rejected below anyway.
            t.parse::<f64>().ok()
        })?;
    v.is_finite().then_some(v)
}

fn meta_number(store: &Store, key: &str) -> Result<Option<f64>, StoreError> {
    Ok(store.get_meta(key)?.and_then(|v| js_finite_number(&v)))
}

/// Read-only snapshot. Any store error (locked, corrupt or newer-schema DB) is returned, never
/// shown as an empty view: the caller keeps its previous snapshot and reports the error. Only a
/// missing meta key (`Ok(None)`) falls back to its default.
pub fn read_snapshot(
    store: &Store,
    daemon_running: bool,
    known_profiles: &[&str],
    now: i64,
) -> Result<HubSnapshot, StoreError> {
    let list = store.list_sessions_counted()?;
    let sessions: Vec<SessionView> = list.sessions.into_iter().map(clean_session).collect();
    let mut profiles: Vec<&str> = Vec::new();
    for p in known_profiles
        .iter()
        .copied()
        .chain(sessions.iter().map(|s| s.profile.as_str()))
    {
        if !profiles.contains(&p) {
            profiles.push(p);
        }
    }
    let mut stale = Vec::new();
    for p in &profiles {
        if let Some(since) = meta_number(store, &format!("stale_since:{p}"))? {
            stale.push(StaleProfile {
                profile: (*p).to_string(),
                since: since as i64,
            });
        }
    }
    let unknown_count = meta_number(store, "unknown_count")?.unwrap_or(0.0) as i64;
    let unknown_version = store
        .get_meta("unknown_version")?
        .filter(|v| !v.is_empty())
        .map(|v| clean_text(&v));
    let stray = meta_number(store, "relay_stray_count")?;
    let stray_recent = meta_number(store, "relay_stray_last")?
        .is_some_and(|last| now as f64 - last <= STRAY_WARN_MS as f64);
    // JS Number.isSafeInteger(stray) && stray > 0
    let relay_stray_count = match stray {
        Some(n) if stray_recent && n.fract() == 0.0 && n > 0.0 && n <= 9_007_199_254_740_991.0 => {
            n as i64
        }
        _ => 0,
    };
    let live: Vec<&SessionView> = sessions.iter().filter(|s| s.ended_at.is_none()).collect();
    let totals = Totals {
        waiting_items: live.iter().map(|s| s.waiting_count).sum(),
        needs_approval: live
            .iter()
            .filter(|s| s.state == LiveStatus::Waiting)
            .count() as i64,
        live: live.len() as i64,
    };
    Ok(HubSnapshot {
        sessions,
        meta: SnapshotMeta {
            unknown_count,
            unknown_version,
            stale,
            daemon: if daemon_running {
                DaemonState::Running
            } else {
                DaemonState::Stopped
            },
            relay_stray_count,
            unreadable_sessions: list.unreadable as i64,
        },
        totals,
    })
}
