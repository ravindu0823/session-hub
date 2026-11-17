//! Classification of one transcript JSONL line. Mirrors `src/ingest/records.ts`.
//! Content of never-store record types is never read.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::text::{JS_SPACE_CLASS, is_js_space};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Classified {
    OwnerPrompt {
        uuid: String,
        ts: String,
        text: String,
    },
    PeerMessage {
        uuid: String,
        ts: String,
        from_name: String,
        from_mode: String,
        body: String,
    },
    AssistantText {
        uuid: String,
        ts: String,
        text: String,
    },
    HeldNotice {
        uuid: String,
        ts: String,
        peer_name: String,
    },
    PermissionMode {
        ts: String,
        mode: String,
    },
    Ignored {
        reason: String,
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        record_type: Option<String>,
    },
    Unknown {
        #[serde(rename = "type")]
        record_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

type Obj = Map<String, Value>;

const NEVER_STORE: &[&str] = &[
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "file-history-snapshot",
    "file-history-delta",
];

const KNOWN_IGNORED: &[&str] = &[
    "queue-operation",
    "last-prompt",
    "mode",
    "ai-title",
    "tools-used",
    "atis-latch",
    "session-owner",
    "pr-link",
    "frame-link",
    "cost-state",
    "artifact-autoreact-ledger",
    "artifact-comment-monitor",
];

const HELD_PREFIX: &str = "Held peer message";
const WRAP_CLOSE: &str = "</cross-session-message>";

static PEER_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"peer claims name: ([^);]+)").expect("peer name regex"));
static WRAP_OPEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<cross-session-message(?-u:\b)([^>]*)>").expect("wrap open regex")
});
static FROM_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?-u:\b)from-name="([^"]*)""#).expect("from-name regex"));
static FROM_MODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?-u:\b)from-mode="([^"]*)""#).expect("from-mode regex"));
/// A user record holding a local command's output rather than anything the owner typed
/// (JS `^\s*<local-command-(?:stdout|stderr)>`).
static LOCAL_COMMAND_OUTPUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        "^[{JS_SPACE_CLASS}]*<local-command-(?:stdout|stderr)>"
    ))
    .expect("local command output regex")
});

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn is_true(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true)))
}

fn ignored(reason: &str) -> Classified {
    Classified::Ignored {
        reason: reason.to_string(),
        record_type: None,
    }
}

fn unknown(r: &Obj, record_type: &str) -> Classified {
    Classified::Unknown {
        record_type: record_type.to_string(),
        version: match r.get("version") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        },
    }
}

/// The message object when its content is a string or an array; None when the shape changed.
fn message_of(r: &Obj) -> Option<&Obj> {
    let Some(Value::Object(msg)) = r.get("message") else {
        return None;
    };
    matches!(msg.get("content"), Some(Value::String(_) | Value::Array(_))).then_some(msg)
}

/// Joined text of `text` blocks, or None when a tool_result block is present.
fn user_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<&str> = Vec::new();
            for b in blocks {
                let Value::Object(b) = b else { continue };
                match b.get("type") {
                    Some(Value::String(t)) if t == "tool_result" => return None,
                    Some(Value::String(t)) if t == "text" => {
                        if let Some(Value::String(s)) = b.get("text") {
                            parts.push(s);
                        }
                    }
                    _ => {}
                }
            }
            Some(parts.join("\n"))
        }
        _ => Some(String::new()),
    }
}

fn assistant_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<&str> = Vec::new();
            for b in blocks {
                let Value::Object(b) = b else { continue };
                if matches!(b.get("type"), Some(Value::String(t)) if t == "text")
                    && let Some(Value::String(s)) = b.get("text")
                {
                    parts.push(s);
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn peer_name(content: &str) -> String {
    PEER_NAME
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim_matches(is_js_space).to_string())
        .unwrap_or_default()
}

fn attr(re: &Regex, attrs: &str) -> String {
    re.captures(attrs)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default()
}

fn idle_peer(r: &Obj, uuid: String, ts: String) -> Classified {
    let Some(msg) = message_of(r) else {
        return unknown(r, "user");
    };
    let Some(text) = user_text(msg.get("content")) else {
        return ignored("tool-result");
    };
    let Some(open) = WRAP_OPEN.captures(&text) else {
        return ignored("peer-unwrapped");
    };
    let whole = open.get(0).expect("whole match");
    let Some(end) = text.rfind(WRAP_CLOSE).filter(|&e| e >= whole.end()) else {
        return ignored("peer-unwrapped");
    };
    let mut body = &text[whole.end()..end];
    body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    body = body
        .strip_suffix("\r\n")
        .or_else(|| body.strip_suffix('\n'))
        .unwrap_or(body);
    let attrs = open.get(1).map_or("", |m| m.as_str());
    Classified::PeerMessage {
        uuid,
        ts,
        from_name: attr(&FROM_NAME, attrs),
        from_mode: attr(&FROM_MODE, attrs),
        body: body.to_string(),
    }
}

fn from_record(r: &Obj, rtype: &str) -> Classified {
    let uuid = str_of(r.get("uuid"));
    let ts = str_of(r.get("timestamp"));
    match rtype {
        "user" => {
            if is_true(r.get("isSidechain")) {
                return ignored("sidechain");
            }
            let origin = r.get("origin");
            if let Some(Value::Object(o)) = origin
                && matches!(o.get("kind"), Some(Value::String(k)) if k == "peer")
            {
                return idle_peer(r, uuid, ts);
            }
            if is_true(r.get("isMeta")) {
                return ignored("meta");
            }
            // `/compact` writes its summary as a user record: Claude Code's text, not the owner's.
            if is_true(r.get("isCompactSummary")) {
                return ignored("compact-summary");
            }
            if let Some(o) = origin {
                let human = matches!(o, Value::Object(o)
                    if matches!(o.get("kind"), Some(Value::String(k)) if k == "human"));
                if !human {
                    return ignored("non-human-origin");
                }
            }
            let Some(msg) = message_of(r) else {
                return unknown(r, rtype);
            };
            match user_text(msg.get("content")) {
                None => ignored("tool-result"),
                Some(t) if t.is_empty() => ignored("empty"),
                // The output of a local command; the owner's `<command-name>` stays a prompt.
                Some(t) if LOCAL_COMMAND_OUTPUT.is_match(&t) => ignored("local-command-output"),
                Some(text) => Classified::OwnerPrompt { uuid, ts, text },
            }
        }
        "assistant" => {
            if is_true(r.get("isSidechain")) {
                return ignored("sidechain");
            }
            let Some(msg) = message_of(r) else {
                return unknown(r, rtype);
            };
            let text = assistant_text(msg.get("content"));
            if text.is_empty() {
                return ignored("empty");
            }
            Classified::AssistantText { uuid, ts, text }
        }
        "attachment" => {
            let Some(Value::Object(a)) = r.get("attachment") else {
                return ignored("attachment");
            };
            if !matches!(a.get("type"), Some(Value::String(t)) if t == "queued_command") {
                return ignored("attachment");
            }
            let Some(o) = a.get("origin") else {
                return ignored("attachment");
            };
            let Value::Object(o) = o else {
                return unknown(r, rtype);
            };
            let kind = o.get("kind");
            if matches!(kind, Some(Value::String(k)) if k == "human") && is_true(a.get("humanTurn"))
            {
                let text = match a.get("prompt") {
                    None => Some(String::new()),
                    p => user_text(p),
                };
                return match text {
                    None => ignored("tool-result"),
                    Some(t) if t.is_empty() => ignored("empty"),
                    Some(text) => Classified::OwnerPrompt { uuid, ts, text },
                };
            }
            if !matches!(kind, Some(Value::String(k)) if k == "peer") {
                return ignored("attachment");
            }
            Classified::PeerMessage {
                uuid,
                ts,
                from_name: str_of(o.get("name")),
                from_mode: str_of(o.get("fromMode")),
                body: str_of(o.get("body")),
            }
        }
        "system" => match r.get("content") {
            Some(Value::String(c)) if c.starts_with(HELD_PREFIX) => Classified::HeldNotice {
                uuid,
                ts,
                peer_name: peer_name(c),
            },
            _ => ignored("system"),
        },
        "permission-mode" => {
            let mode = str_of(r.get("permissionMode"));
            if mode.is_empty() {
                return ignored("empty");
            }
            Classified::PermissionMode {
                ts: String::new(),
                mode,
            }
        }
        _ => Classified::Ignored {
            reason: "unrecognized-type".to_string(),
            record_type: Some(rtype.to_string()),
        },
    }
}

fn hex4(b: &[u8]) -> Option<u16> {
    if b.len() < 4 {
        return None;
    }
    let mut v: u16 = 0;
    for &c in &b[..4] {
        v = (v << 4) | u16::try_from(char::from(c).to_digit(16)?).ok()?;
    }
    Some(v)
}

/// JS `JSON.parse` accepts unpaired `\uD800`-`\uDFFF` escapes (as lone surrogates); serde_json
/// rejects them. Rewrite each unpaired one to `\uFFFD`; valid pairs and escaped backslashes are
/// left alone. Borrows when nothing changes.
pub(crate) fn replace_lone_surrogates(line: &str) -> Cow<'_, str> {
    let b = line.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        if b.get(i + 1) != Some(&b'u') {
            i += 2;
            continue;
        }
        let Some(u) = hex4(&b[i + 2..]) else {
            i += 2;
            continue;
        };
        let high_pair = (0xD800..0xDC00).contains(&u)
            && b.get(i + 6) == Some(&b'\\')
            && b.get(i + 7) == Some(&b'u')
            && hex4(&b[i + 8..]).is_some_and(|l| (0xDC00..0xE000).contains(&l));
        if high_pair {
            i += 12;
        } else if (0xD800..0xE000).contains(&u) {
            let o = out.get_or_insert_with(String::new);
            o.push_str(&line[copied..i]);
            o.push_str("\\uFFFD");
            i += 6;
            copied = i;
        } else {
            i += 6;
        }
    }
    match out {
        Some(mut o) => {
            o.push_str(&line[copied..]);
            Cow::Owned(o)
        }
        None => Cow::Borrowed(line),
    }
}

/// Classify one transcript JSONL line. Never panics.
pub fn classify(line: &str) -> Classified {
    let line = replace_lone_surrogates(line);
    let Ok(Value::Object(rec)) = serde_json::from_str::<Value>(&line) else {
        return ignored("bad-json");
    };
    let rtype = str_of(rec.get("type"));
    if NEVER_STORE.contains(&rtype.as_str()) {
        return ignored("never-store");
    }
    if KNOWN_IGNORED.contains(&rtype.as_str()) {
        return ignored("not-needed");
    }
    from_record(&rec, &rtype)
}
