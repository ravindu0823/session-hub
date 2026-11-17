mod common;

use std::time::{Duration, Instant};

use hub_core::security::{clean_input, clean_text, looks_secret, mask_secrets};

fn str_of<'a>(v: &'a serde_json::Value, what: &str, case: &str) -> &'a str {
    v.as_str()
        .unwrap_or_else(|| panic!("{case}: {what} is not a string"))
}

#[test]
fn mask_matches_goldens() {
    let cases = common::golden("mask");
    let mut failed = Vec::new();
    for c in &cases {
        let input = str_of(&c.input, "input", &c.name);
        let want = str_of(&c.output, "output", &c.name);
        let got = mask_secrets(input);
        if got != want {
            failed.push(format!(
                "{}\n  input: {input:?}\n  want:  {want:?}\n  got:   {got:?}",
                c.name
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} mask goldens differ:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}

#[test]
fn mask_is_idempotent() {
    for c in common::golden("mask") {
        let once = str_of(&c.output, "output", &c.name);
        assert_eq!(mask_secrets(once), once, "{}", c.name);
    }
}

#[test]
fn looks_secret_is_mask_changes_text() {
    for c in common::golden("mask") {
        let input = str_of(&c.input, "input", &c.name);
        let changed = str_of(&c.output, "output", &c.name) != input;
        assert_eq!(looks_secret(input), changed, "{}", c.name);
    }
}

#[test]
fn clean_matches_goldens() {
    let cases = common::golden("clean");
    let mut failed = Vec::new();
    for c in &cases {
        let s = str_of(&c.input["s"], "input.s", &c.name);
        let want = str_of(&c.output, "output", &c.name);
        let got = match c.input["fn"].as_str() {
            Some("text") => clean_text(s),
            Some("input") => clean_input(s, c.input["meta"].as_bool().unwrap_or(false)),
            other => panic!("{}: unknown fn {other:?}", c.name),
        };
        if got != want {
            failed.push(format!(
                "{}\n  input: {s:?}\n  want:  {want:?}\n  got:   {got:?}",
                c.name
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} clean goldens differ:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}

#[test]
fn clean_text_maps_esc_before_an_astral_char_like_utf8_output_of_ts() {
    // TS drops ESC plus the high surrogate and leaves a lone low surrogate, which is written to the
    // terminal as U+FFFD. A Rust String cannot hold the lone surrogate, so it holds U+FFFD.
    assert_eq!(clean_text("a\u{1b}\u{1F600}b"), "a\u{FFFD}b");
    assert_eq!(clean_text("a\u{1b}\u{e9}b"), "ab");
}

// ---- linear-time gate (port of expectLinear in test/security/mask.test.ts) ----

/// SESSION_HUB_NO_ABS_TIMING=1 drops the absolute 1 s cap (slow or shared runners); the ratio gate stays.
fn abs_timing() -> bool {
    std::env::var("SESSION_HUB_NO_ABS_TIMING").as_deref() != Ok("1")
}

fn time_calls(input: &str, calls: u32) -> Duration {
    let t0 = Instant::now();
    for _ in 0..calls {
        std::hint::black_box(mask_secrets(std::hint::black_box(input)));
    }
    t0.elapsed()
}

/// 8x the input must cost less than 24x the time (linear is about 8x, quadratic about 64x), with a
/// 1 s cap for 1 MB. Best of at least 3 alternating rounds; while the ratio is still 24 or more, up
/// to 7 more rounds may lower the minimums. The 125 KB time is 8 calls / 8, floored at 1 ms.
fn expect_linear(what: &str, make: impl Fn(usize) -> String) {
    let big = make(1_000_000);
    let little = make(125_000);
    mask_secrets(&big);
    mask_secrets(&little);
    let mut large = f64::INFINITY;
    let mut small = f64::INFINITY;
    let ratio = |large: f64, small: f64| large / small.max(1.0);
    let mut i = 0;
    while i < 10 && (i < 3 || ratio(large, small) >= 24.0) {
        large = large.min(time_calls(&big, 1).as_secs_f64() * 1000.0);
        small = small.min(time_calls(&little, 8).as_secs_f64() * 1000.0 / 8.0);
        i += 1;
    }
    eprintln!(
        "linear {what:?}: 1 MB {large:.2} ms, 125 KB {small:.3} ms, ratio {:.2}",
        ratio(large, small)
    );
    if abs_timing() {
        assert!(large < 1000.0, "{what:?}: 1 MB took {large:.1} ms");
    }
    assert!(
        ratio(large, small) < 24.0,
        "{what:?}: ratio {:.1} (1 MB {large:.1} ms, 125 KB {small:.2} ms)",
        ratio(large, small)
    );
}

fn repeat_to(unit: &str) -> impl Fn(usize) -> String + '_ {
    move |bytes| unit.repeat(bytes.div_ceil(unit.len()))
}

fn secrets_fixture(name: &str) -> String {
    let p = common::fixture(&format!("secrets/{name}"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
#[ignore = "timing: run in release with `-- --ignored mask_is_linear`"]
fn mask_is_linear() {
    let units = [
        // adversarial camelCase input
        "aToken=",
        "aToken",
        "aAb",
        "dbPassword ",
        // adversarial value scan
        "password=",
        "password=[b",
        "password=ab[masked:",
        "token=\"",
        "aToken='x",
        "[b",
        // adversarial input
        "eyJ",
        "Server=a;",
        "-----BEGIN PRIVATE KEY-----\n",
        "password_",
        "a_",
        "token ",
        "a.",
        "x://a:",
    ];
    for unit in units {
        expect_linear(unit, repeat_to(unit));
    }
    for tail in ["Token", "Token=", "Token: x", "Tokens=abcdefgh"] {
        let make = |bytes: usize| format!("{}{tail}", "aAb".repeat(bytes.div_ceil(3)));
        expect_linear(&format!("aAb...{tail}"), make);
        assert!(mask_secrets(&make(1_000_000)).len() > 1_000_000, "{tail}");
    }
    let chunk = format!(
        "{}{}{}{}prose line here ",
        secrets_fixture("github.txt"),
        secrets_fixture("password-kv.txt"),
        secrets_fixture("safe.txt"),
        secrets_fixture("conn-string.txt")
    );
    expect_linear("mixed text", repeat_to(&chunk));
}

#[test]
#[ignore = "timing: run in release with `-- --ignored mask_is_linear`"]
fn mask_is_linear_on_long_digit_and_letter_runs() {
    let big = format!("{} {}", "4".repeat(500_000), "GB82 ".repeat(100_000));
    let t0 = Instant::now();
    mask_secrets(&big);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    eprintln!("long digit and letter runs: {ms:.2} ms");
    assert!(
        ms < if abs_timing() { 2_000.0 } else { 30_000.0 },
        "{ms:.1} ms"
    );
}
