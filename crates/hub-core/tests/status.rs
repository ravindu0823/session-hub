mod common;

use std::time::{Duration, Instant};

use hub_core::status::{Status, WaitingItem, answered_ids, extract_status};

fn strings(v: &serde_json::Value, what: &str, case: &str) -> Vec<String> {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{case}: {what}: {e}"))
}

#[test]
fn extract_matches_goldens() {
    let cases = common::golden("extract");
    let mut failed = Vec::new();
    for c in &cases {
        let input = c
            .input
            .as_str()
            .unwrap_or_else(|| panic!("{}: input is not a string", c.name));
        let got = serde_json::to_value(extract_status(input)).expect("serialize");
        if got != c.output {
            failed.push(format!(
                "{}\n  input: {input:?}\n  want:  {}\n  got:   {got}",
                c.name, c.output
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} extract goldens differ:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}

#[test]
fn answered_matches_goldens() {
    let cases = common::golden("answered");
    let mut failed = Vec::new();
    for c in &cases {
        let listed = strings(&c.input["listed"], "listed", &c.name);
        let texts = strings(&c.input["texts"], "texts", &c.name);
        let want = strings(&c.output, "output", &c.name);
        let got: Vec<String> = answered_ids(&listed, &texts).into_iter().collect();
        if got != want {
            failed.push(format!(
                "{}\n  listed: {listed:?}\n  want:   {want:?}\n  got:    {got:?}",
                c.name
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} answered goldens differ:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}

#[test]
fn status_round_trips_as_camel_case_json() {
    let status = Status {
        progress_pct: Some(42),
        phase: Some("build".into()),
        remaining: vec!["a".into()],
        waiting: vec![WaitingItem {
            id: Some("Q1".into()),
            text: "t".into(),
            is_new: true,
            children: vec!["c".into()],
        }],
        recommendations: vec![],
    };
    let json = serde_json::to_string(&status).expect("serialize");
    assert_eq!(
        json,
        r#"{"progressPct":42,"phase":"build","remaining":["a"],"waiting":[{"id":"Q1","text":"t","isNew":true,"children":["c"]}],"recommendations":[]}"#
    );
    let back: Status = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, status);
}

#[test]
fn answered_with_empty_list_is_empty() {
    assert!(answered_ids(&[], &["Q1 yes".to_string()]).is_empty());
}

/// Hostile input stays fast: long runs of ID-like text, underscores and stars.
#[test]
fn hostile_input_is_bounded() {
    let long_id = "A-".repeat(20_000) + "A1";
    let stars = "*a".repeat(20_000);
    let unders = "_a".repeat(20_000);
    let md = format!(
        "**Progress: 5% overall. Phase: x.**\n### Waiting on you\n- {long_id}\n- {stars}\n- {unders}\n"
    );
    let started = Instant::now();
    assert!(extract_status(&md).is_some());
    let texts = vec![format!("{long_id} {long_id}"), "Q1 / ".repeat(20_000)];
    answered_ids(&["Q1".to_string()], &texts);
    assert!(started.elapsed() < Duration::from_secs(10));
}
