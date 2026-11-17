//! `hub.db` schema: a verbatim copy of `MIGRATIONS` in `src/store/schema.ts`. The TS and Rust stores
//! share one database file, so every string here must stay byte-identical to the TS original.

/// Ordered migrations. Index i holds schema version i + 1. Never edit a shipped entry; append.
pub const MIGRATIONS: [&str; 5] = [
    r#"
CREATE TABLE sessions (
  session_id TEXT PRIMARY KEY,
  profile TEXT NOT NULL,
  name TEXT NOT NULL,
  cwd TEXT NOT NULL,
  pid INTEGER NOT NULL,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  ended_at INTEGER,
  transcript_path TEXT NOT NULL,
  permission_mode TEXT
);

CREATE TABLE tails (
  path TEXT PRIMARY KEY,
  offset INTEGER NOT NULL,
  size INTEGER NOT NULL,
  first_uuid TEXT
);

CREATE TABLE messages (
  id INTEGER PRIMARY KEY,
  uuid TEXT NOT NULL UNIQUE,
  session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
  ts TEXT NOT NULL,
  role TEXT NOT NULL,
  from_name TEXT,
  text TEXT NOT NULL
);
CREATE INDEX messages_session_ts ON messages(session_id, ts);

CREATE VIRTUAL TABLE messages_fts USING fts5(text, content='messages', content_rowid='id');
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE TABLE status (
  session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
  progress_pct REAL,
  phase TEXT,
  remaining_json TEXT NOT NULL,
  waiting_json TEXT NOT NULL,
  recommendations_json TEXT NOT NULL,
  reply_uuid TEXT NOT NULL,
  reply_ts TEXT NOT NULL
);

CREATE TABLE latest_reply (
  session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
  reply_uuid TEXT NOT NULL,
  reply_ts TEXT NOT NULL,
  has_status INTEGER NOT NULL
);

CREATE TABLE sends (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  profile TEXT NOT NULL,
  name TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  state TEXT NOT NULL,
  detail TEXT
);
CREATE INDEX sends_session_state ON sends(session_id, state);

CREATE TABLE meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#,
    // v2: unrecognised record types no longer count as unknown; drop the count v1 inflated with them.
    r#"
UPDATE meta SET value = '0' WHERE key = 'unknown_count';
"#,
    // v3: deleted message text must not stay readable in the full-text index (PII-03). The option is
    // stored in the index; 'optimize' rewrites the existing segments so earlier deletes go too.
    r#"
INSERT INTO messages_fts(messages_fts, rank) VALUES ('secure-delete', 1);
INSERT INTO messages_fts(messages_fts) VALUES ('optimize');
CREATE INDEX messages_ts ON messages(ts);
"#,
    // v4: retention and history order compare timestamps as text, so every stored one must be ISO UTC
    // with milliseconds, as ingest writes them since DATA-07. Rows from older builds can hold an empty
    // string, an offset or other text. A date-like value is converted to UTC; an empty or unparseable
    // one takes its session's updated_at (now, if the session is missing). Only rows that change are
    // rewritten. The full-text update trigger first narrows to text changes: as shipped it re-indexed
    // a row on any update, which made this rewrite take about a minute per 200k changed rows.
    r#"
DROP TRIGGER messages_au;
CREATE TRIGGER messages_au AFTER UPDATE OF text ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;

UPDATE messages SET ts = strftime('%Y-%m-%dT%H:%M:%fZ', ts)
 WHERE ts GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*'
   AND strftime('%Y-%m-%dT%H:%M:%fZ', ts) IS NOT NULL
   AND ts <> strftime('%Y-%m-%dT%H:%M:%fZ', ts);
UPDATE messages SET ts = COALESCE(
  (SELECT strftime('%Y-%m-%dT%H:%M:%fZ', s.updated_at / 1000.0, 'unixepoch')
     FROM sessions s WHERE s.session_id = messages.session_id),
  strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
 WHERE strftime('%Y-%m-%dT%H:%M:%fZ', ts) IS NOT ts
    OR NOT ts GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*';

UPDATE status SET reply_ts = strftime('%Y-%m-%dT%H:%M:%fZ', reply_ts)
 WHERE reply_ts GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*'
   AND strftime('%Y-%m-%dT%H:%M:%fZ', reply_ts) IS NOT NULL
   AND reply_ts <> strftime('%Y-%m-%dT%H:%M:%fZ', reply_ts);
UPDATE status SET reply_ts = COALESCE(
  (SELECT strftime('%Y-%m-%dT%H:%M:%fZ', s.updated_at / 1000.0, 'unixepoch')
     FROM sessions s WHERE s.session_id = status.session_id),
  strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
 WHERE strftime('%Y-%m-%dT%H:%M:%fZ', reply_ts) IS NOT reply_ts
    OR NOT reply_ts GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*';

UPDATE latest_reply SET reply_ts = strftime('%Y-%m-%dT%H:%M:%fZ', reply_ts)
 WHERE reply_ts GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*'
   AND strftime('%Y-%m-%dT%H:%M:%fZ', reply_ts) IS NOT NULL
   AND reply_ts <> strftime('%Y-%m-%dT%H:%M:%fZ', reply_ts);
UPDATE latest_reply SET reply_ts = COALESCE(
  (SELECT strftime('%Y-%m-%dT%H:%M:%fZ', s.updated_at / 1000.0, 'unixepoch')
     FROM sessions s WHERE s.session_id = latest_reply.session_id),
  strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
 WHERE strftime('%Y-%m-%dT%H:%M:%fZ', reply_ts) IS NOT reply_ts
    OR NOT reply_ts GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*';
"#,
    r#"
DELETE FROM messages
 WHERE role = 'owner'
   AND (text LIKE '<local-command-stdout>%'
     OR text LIKE '<local-command-stderr>%'
     OR text LIKE 'This session is being continued from a previous conversation%');
"#,
];
