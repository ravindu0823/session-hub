mod common;

use hub_core::ingest::records::classify;

#[test]
fn records_match_goldens() {
    let cases = common::golden("records");
    assert!(!cases.is_empty());
    for c in &cases {
        let input = c.input.as_str().expect("input is a string");
        let got = serde_json::to_value(classify(input)).unwrap();
        assert_eq!(got, c.output, "{}", c.name);
    }
}

#[test]
fn never_store_types_carry_no_content() {
    for t in [
        "api-request",
        "api-request-blob",
        "api-request-shape",
        "file-history-snapshot",
        "file-history-delta",
    ] {
        let line = format!(
            r#"{{"type":"{t}","uuid":"u","body":"SECRET-BODY","message":{{"content":"SECRET-BODY"}}}}"#
        );
        let out = format!("{:?}", classify(&line));
        assert!(!out.contains("SECRET-BODY"), "{t}: {out}");
        assert!(out.contains("never-store"), "{t}");
    }
}

fn user(content: &str) -> String {
    format!(r#"{{"type":"user","uuid":"u","timestamp":"t","message":{{"content":"{content}"}}}}"#)
}

fn owner_text(line: &str) -> String {
    match classify(line) {
        hub_core::ingest::records::Classified::OwnerPrompt { text, .. } => text,
        other => panic!("not an owner prompt: {other:?}"),
    }
}

#[test]
fn lone_high_surrogate_becomes_replacement_char() {
    assert_eq!(owner_text(&user(r"a\ud83d b")), "a\u{FFFD} b");
    assert_eq!(owner_text(&user(r"a\uD83D")), "a\u{FFFD}");
}

#[test]
fn lone_low_surrogate_becomes_replacement_char() {
    assert_eq!(owner_text(&user(r"a\ude00b")), "a\u{FFFD}b");
    assert_eq!(owner_text(&user(r"\ud83d😀")), "\u{FFFD}\u{1F600}");
}

#[test]
fn valid_surrogate_pair_is_unchanged() {
    // The escaped pair (a writer that escapes non-ASCII) and the literal character.
    assert_eq!(owner_text(&user(r"\ud83d\ude00")), "\u{1F600}");
    assert_eq!(owner_text(&user(r"a\uD83D\uDE00b")), "a\u{1F600}b");
    assert_eq!(owner_text(&user("😀")), "\u{1F600}");
}

#[test]
fn escaped_backslash_is_not_an_escape() {
    assert_eq!(owner_text(&user(r"\\ud83d")), r"\ud83d");
    assert_eq!(owner_text(&user(r"\\\ud83d")), "\\\u{FFFD}");
}

#[test]
fn lone_surrogate_in_assistant_text_block() {
    let line = r#"{"type":"assistant","uuid":"a","timestamp":"t","message":{"content":[{"type":"text","text":"x\ud800y"}]}}"#;
    match classify(line) {
        hub_core::ingest::records::Classified::AssistantText { text, .. } => {
            assert_eq!(text, "x\u{FFFD}y")
        }
        other => panic!("{other:?}"),
    }
}

// Port of "a compaction summary and local command output are not owner prompts; a typed command is".
#[test]
fn a_compaction_summary_and_local_command_output_are_not_owner_prompts_a_typed_command_is() {
    use hub_core::ingest::records::Classified;
    let raw = std::fs::read_to_string(common::fixture("records/user-system.jsonl")).unwrap();
    let r: Vec<Classified> = raw.lines().map(classify).collect();
    let ignored = |reason: &str| Classified::Ignored {
        reason: reason.to_string(),
        record_type: None,
    };
    assert_eq!(r[0], ignored("compact-summary"));
    assert_eq!(r[1], ignored("local-command-output"));
    assert_eq!(r[2], ignored("local-command-output"));
    assert_eq!(r[3], ignored("local-command-output"));
    assert!(matches!(r[4], Classified::OwnerPrompt { .. }));
    // Only text that starts with the tag is command output; the owner may mention the tag.
    match &r[5] {
        Classified::OwnerPrompt { text, .. } => {
            assert_eq!(text, "Q7 yes, and see <local-command-stdout> above");
        }
        other => panic!("{other:?}"),
    }
    assert!(!format!("{:?}", &r[..4]).contains("fixture"));
}

#[test]
fn local_command_output_tag_may_follow_js_whitespace_only() {
    use hub_core::ingest::records::Classified;
    // JS `\s` includes U+FEFF and U+00A0 (and not U+0085).
    for lead in ["", " ", "\\n", "\\ufeff", "\\u00a0"] {
        let line = user(&format!("{lead}<local-command-stdout>x"));
        assert!(
            matches!(classify(&line), Classified::Ignored { ref reason, .. } if reason == "local-command-output"),
            "{lead:?}"
        );
    }
    for lead in ["\\u0085", "a "] {
        let line = user(&format!("{lead}<local-command-stdout>x"));
        assert!(
            matches!(classify(&line), Classified::OwnerPrompt { .. }),
            "{lead:?}"
        );
    }
}

#[test]
fn compact_summary_flag_is_ignored_only_when_exactly_true() {
    use hub_core::ingest::records::Classified;
    let mk = |flag: &str| {
        format!(
            r#"{{"type":"user","uuid":"u","timestamp":"t","isCompactSummary":{flag},"message":{{"content":"hi"}}}}"#
        )
    };
    assert!(
        matches!(classify(&mk("true")), Classified::Ignored { ref reason, .. } if reason == "compact-summary")
    );
    assert!(matches!(
        classify(&mk("\"true\"")),
        Classified::OwnerPrompt { .. }
    ));
    assert!(matches!(
        classify(&mk("false")),
        Classified::OwnerPrompt { .. }
    ));
}
