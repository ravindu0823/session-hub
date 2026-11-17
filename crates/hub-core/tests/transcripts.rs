//! Tests of `daemon::transcripts` (TS `src/daemon/transcripts.ts` has no dedicated test file).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use hub_core::daemon::transcripts::{
    first_cwd, is_session_id, list_transcripts, node_normalize, transcript_index,
};
use tempfile::TempDir;

const SID: &str = "11111111-2222-4333-8444-555555555555";

fn tmp() -> TempDir {
    tempfile::Builder::new()
        .prefix("hub-tr-")
        .tempdir()
        .unwrap()
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn sorted(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    v.sort();
    v
}

// ---------------------------------------------------------------- is_session_id

#[test]
fn is_session_id_accepts_36_hex_or_dash_chars_in_either_case() {
    assert!(is_session_id(SID));
    assert!(is_session_id("ABCDEF00-2222-4333-8444-555555555555"));
    // The pattern is the character set and length only, like TS.
    assert!(is_session_id(&"-".repeat(36)));
    assert!(is_session_id(&"a".repeat(36)));
}

#[test]
fn is_session_id_refuses_anything_else() {
    for bad in [
        "",
        "11111111-2222-4333-8444-55555555555",   // 35
        "11111111-2222-4333-8444-5555555555555", // 37
        "11111111-2222-4333-8444-55555555555g",
        "11111111-2222-4333-8444-55555555555\n",
        "\n11111111-2222-4333-8444-555555555555",
        "11111111-2222-4333-8444-555555555555\n",
        "../../../../../../../../../../etc/pwd",
        "11111111_2222_4333_8444_555555555555",
        // Full-width and Arabic-Indic digits are not [0-9].
        "１1111111-2222-4333-8444-55555555555",
        "٤1111111-2222-4333-8444-555555555555",
    ] {
        assert!(!is_session_id(bad), "{bad:?}");
    }
}

// ---------------------------------------------------------------- list_transcripts

#[test]
fn lists_jsonl_files_one_level_below_projects_only() {
    let d = tmp();
    let projects = d.path().join("projects");
    let a = projects.join("C--a").join(format!("{SID}.jsonl"));
    let b = projects.join("C--b").join("other.jsonl");
    write(&a, "{}\n");
    write(&b, "{}\n");
    // Not listed: deeper, at the projects root, other extensions, a folder named *.jsonl.
    write(
        &projects.join("C--a").join("sub").join("deep.jsonl"),
        "{}\n",
    );
    write(&projects.join("root.jsonl"), "{}\n");
    write(&projects.join("C--a").join("notes.json"), "{}\n");
    write(&projects.join("C--a").join("x.JSONL"), "{}\n");
    write(&projects.join("C--a").join("x.jsonl.bak"), "{}\n");
    std::fs::create_dir_all(projects.join("C--a").join("dir.jsonl")).unwrap();
    // Outside projects.
    write(&d.path().join("sessions").join("x.jsonl"), "{}\n");
    let got = sorted(list_transcripts(d.path()).unwrap());
    assert_eq!(got, sorted(vec![a, b]));
}

#[test]
fn a_profile_without_projects_lists_nothing() {
    let d = tmp();
    assert!(list_transcripts(d.path()).unwrap().is_empty());
    assert!(
        list_transcripts(&d.path().join("missing"))
            .unwrap()
            .is_empty()
    );
    // projects is a file: nothing.
    std::fs::write(d.path().join("projects"), "x").unwrap();
    assert!(list_transcripts(d.path()).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed() {
    let d = tmp();
    let outside = d.path().join("outside");
    write(&outside.join("x.jsonl"), "{}\n");
    let projects = d.path().join("projects");
    std::fs::create_dir_all(projects.join("real")).unwrap();
    std::os::unix::fs::symlink(&outside, projects.join("link")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("x.jsonl"),
        projects.join("real").join("l.jsonl"),
    )
    .unwrap();
    assert!(list_transcripts(d.path()).unwrap().is_empty());
}

// ---------------------------------------------------------------- transcript_index

#[test]
fn transcript_index_maps_stem_to_path_first_listed_wins() {
    let d = tmp();
    let projects = d.path().join("projects");
    let a = projects.join("p1").join(format!("{SID}.jsonl"));
    let b = projects.join("p2").join(format!("{SID}.jsonl"));
    let c = projects.join("p2").join("other.jsonl");
    write(&a, "");
    write(&b, "");
    write(&c, "");
    let listed = list_transcripts(d.path()).unwrap();
    let first = listed
        .iter()
        .find(|p| p.file_name().unwrap() == format!("{SID}.jsonl").as_str())
        .unwrap()
        .clone();
    let idx: HashMap<String, PathBuf> = transcript_index(d.path()).unwrap();
    assert_eq!(idx.len(), 2);
    assert_eq!(idx[SID], first);
    assert_eq!(idx["other"], c);
}

// ---------------------------------------------------------------- first_cwd

#[test]
fn first_cwd_is_the_cwd_of_the_first_record_that_has_a_string_one() {
    let d = tmp();
    let p = d.path().join("t.jsonl");
    write(
        &p,
        concat!(
            "not json\n",
            "{\"type\":\"x\"}\n",
            "[\"cwd\"]\n",
            "{\"cwd\":42}\n",
            "{\"cwd\":\"C:\\\\fixture\\\\a\"}\n",
            "{\"cwd\":\"/second\"}\n",
        ),
    );
    assert_eq!(first_cwd(&p).unwrap(), "C:\\fixture\\a");
}

#[test]
fn first_cwd_reads_crlf_lines_and_a_bom() {
    let d = tmp();
    let p = d.path().join("t.jsonl");
    write(&p, "\u{FEFF}{\"cwd\":\"/a\"}\r\n{\"cwd\":\"/b\"}\r\n");
    assert_eq!(first_cwd(&p).unwrap(), "/a");
}

#[test]
fn first_cwd_skips_a_last_line_that_may_be_cut() {
    let d = tmp();
    let p = d.path().join("t.jsonl");
    write(&p, "{\"type\":\"x\"}\n{\"cwd\":\"/half-written\"}");
    assert_eq!(first_cwd(&p).unwrap(), "");
    // A single line without a newline is still read (TS pops only when there are two or more).
    write(&p, "{\"cwd\":\"/only\"}");
    assert_eq!(first_cwd(&p).unwrap(), "/only");
    write(&p, "");
    assert_eq!(first_cwd(&p).unwrap(), "");
}

#[test]
fn first_cwd_keeps_a_record_with_a_lone_surrogate_escape() {
    let d = tmp();
    let p = d.path().join("t.jsonl");
    write(&p, "{\"x\":\"\\ud800\",\"cwd\":\"/lone\"}\n");
    assert_eq!(first_cwd(&p).unwrap(), "/lone");
}

#[test]
fn first_cwd_looks_only_at_the_first_256_kb() {
    let d = tmp();
    let p = d.path().join("t.jsonl");
    let pad = format!("{{\"pad\":\"{}\"}}\n", "x".repeat(1000));
    let mut f = std::fs::File::create(&p).unwrap();
    let mut written = 0;
    while written < 256 * 1024 {
        f.write_all(pad.as_bytes()).unwrap();
        written += pad.len();
    }
    f.write_all(b"{\"cwd\":\"/late\"}\n").unwrap();
    drop(f);
    assert_eq!(first_cwd(&p).unwrap(), "");

    // A record that ends inside the 256 KB head counts.
    let mut f = std::fs::File::create(&p).unwrap();
    let mut written = 0;
    let line = "{\"cwd\":\"/in\"}\n";
    while written + pad.len() + line.len() < 256 * 1024 {
        f.write_all(pad.as_bytes()).unwrap();
        written += pad.len();
    }
    f.write_all(line.as_bytes()).unwrap();
    f.write_all(pad.as_bytes()).unwrap();
    drop(f);
    assert_eq!(first_cwd(&p).unwrap(), "/in");
}

#[test]
fn first_cwd_of_a_missing_file_is_an_error() {
    let d = tmp();
    assert!(first_cwd(&d.path().join("missing.jsonl")).is_err());
}

#[test]
fn node_normalize_is_path_normalize() {
    let n = |s: &str| node_normalize(Path::new(s)).to_string_lossy().into_owned();
    if cfg!(windows) {
        assert_eq!(n("C:/Users/u/.claude"), r"C:\Users\u\.claude");
        assert_eq!(n(r"C:\a\.\b\..\c"), r"C:\a\c");
        assert_eq!(n("C:/.."), r"C:\");
        assert_eq!(n(r"\\server\share\x"), r"\\server\share\x");
    } else {
        assert_eq!(n("/home/u//.claude/./x"), "/home/u/.claude/x");
        assert_eq!(n("/a/b/../c"), "/a/c");
        assert_eq!(n("/.."), "/");
    }
    assert_eq!(n("a/../.."), "..");
    assert_eq!(n("./"), ".");
}

#[test]
fn listed_paths_use_the_normalised_profile_dir() {
    let d = tmp();
    let p = d
        .path()
        .join("projects")
        .join("C--x")
        .join(format!("{SID}.jsonl"));
    write(&p, "{}\n");
    let odd = if cfg!(windows) {
        d.path().to_string_lossy().replace('\\', "/")
    } else {
        format!("{}/.//", d.path().to_string_lossy())
    };
    assert_eq!(list_transcripts(Path::new(&odd)).unwrap(), vec![p]);
}
