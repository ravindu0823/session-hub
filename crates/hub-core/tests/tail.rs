use hub_core::config::FIRST_INDEX_BYTES;
use hub_core::ingest::tail::{TailState, initial_state, read_new};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn file(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
    let p = dir.path().join(name);
    fs::write(&p, content).unwrap();
    p
}
fn append(p: &Path, content: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(p)
        .unwrap()
        .write_all(content)
        .unwrap();
}
fn fresh(p: &Path) -> TailState {
    TailState {
        path: p.to_string_lossy().into_owned(),
        offset: 0,
        size: 0,
        first_uuid: None,
    }
}
fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn returns_only_complete_lines() {
    let d = TempDir::new().unwrap();
    let p = file(
        &d,
        "a",
        b"{\"uuid\":\"a\"}\n{\"uuid\":\"b\"}\n{\"uuid\":\"c\"",
    );
    let r = read_new(&fresh(&p), 1024).unwrap();
    assert_eq!(r.lines, s(&["{\"uuid\":\"a\"}", "{\"uuid\":\"b\"}"]));
    assert!(!r.reset);
    assert_eq!(r.next.first_uuid.as_deref(), Some("a"));
}

#[test]
fn keeps_partial_line_for_next_read() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"uuid\":\"a\"}\n{\"uuid\":\"b\"");
    let r1 = read_new(&fresh(&p), 1024).unwrap();
    assert_eq!(r1.lines, s(&["{\"uuid\":\"a\"}"]));
    append(&p, b"}\n{\"uuid\":\"c\"}\n");
    let r2 = read_new(&r1.next, 1024).unwrap();
    assert_eq!(r2.lines, s(&["{\"uuid\":\"b\"}", "{\"uuid\":\"c\"}"]));
    assert!(!r2.reset);
    let r3 = read_new(&r2.next, 1024).unwrap();
    assert!(r3.lines.is_empty());
    assert_eq!(r3.next.offset, r2.next.offset);
}

#[test]
fn shrunk_file_resets() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"uuid\":\"a\"}\n{\"uuid\":\"b\"}\n");
    let r1 = read_new(&fresh(&p), 1024).unwrap();
    fs::write(&p, b"{\"uuid\":\"a\"}\n").unwrap();
    let r2 = read_new(&r1.next, 1024).unwrap();
    assert!(r2.reset);
    assert!(r2.lines.is_empty());
    assert_eq!(r2.next.offset, 0);
    let r3 = read_new(&r2.next, 1024).unwrap();
    assert_eq!(r3.lines, s(&["{\"uuid\":\"a\"}"]));
    assert!(!r3.reset);
}

#[test]
fn replaced_file_resets() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"uuid\":\"a\"}\n{\"uuid\":\"b\"}\n");
    let r1 = read_new(&fresh(&p), 1024).unwrap();
    fs::write(
        &p,
        b"{\"uuid\":\"x\"}\n{\"uuid\":\"y\"}\n{\"uuid\":\"z\"}\n",
    )
    .unwrap();
    let r2 = read_new(&r1.next, 1024).unwrap();
    assert!(r2.reset);
    assert_eq!(r2.next.offset, 0);
    assert_eq!(r2.next.first_uuid, None);
}

#[test]
fn skips_leading_lines_without_uuid() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"type\":\"x\"}\nnot json\n{\"uuid\":\"q\"}\n");
    let r = read_new(&fresh(&p), 1024).unwrap();
    assert_eq!(r.next.first_uuid.as_deref(), Some("q"));
    assert_eq!(r.lines.len(), 3);
}

#[test]
fn non_string_uuid_is_skipped() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"uuid\":5}\n{\"uuid\":\"q\"}\n");
    let r = read_new(&fresh(&p), 1024).unwrap();
    assert_eq!(r.next.first_uuid.as_deref(), Some("q"));
}

#[test]
fn large_file_starts_at_tail_and_aligns() {
    let d = TempDir::new().unwrap();
    let lines: Vec<String> = (0..200)
        .map(|i| format!("{{\"uuid\":\"u{i}\",\"pad\":\"{}\"}}", "x".repeat(40)))
        .collect();
    let p = file(&d, "a", format!("{}\n", lines.join("\n")).as_bytes());
    let st = initial_state(&p, 1000).unwrap();
    assert!(st.offset > 0);
    let r = read_new(&st, 4096).unwrap();
    assert!(!r.reset);
    assert!(r.lines.len() > 5 && r.lines.len() < 200);
    for l in &r.lines {
        serde_json::from_str::<serde_json::Value>(l).unwrap();
    }
    assert_eq!(r.lines.last().unwrap(), &lines[199]);
    assert_eq!(r.next.first_uuid.as_deref(), Some("u0"));
}

#[test]
fn initial_state_offset_zero_for_small_file() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"uuid\":\"a\"}\n");
    assert_eq!(initial_state(&p, 1000).unwrap().offset, 0);
}

#[test]
fn crlf_and_multibyte_across_chunk_boundaries() {
    let d = TempDir::new().unwrap();
    let p = file(
        &d,
        "a",
        "{\"uuid\":\"a\",\"t\":\"héllo €😀\"}\r\n\r\n{\"uuid\":\"b\",\"t\":\"ü\"}\r\n".as_bytes(),
    );
    let all = read_new(&fresh(&p), 4096).unwrap();
    assert_eq!(
        all.lines,
        s(&[
            "{\"uuid\":\"a\",\"t\":\"héllo €😀\"}",
            "{\"uuid\":\"b\",\"t\":\"ü\"}"
        ])
    );
    for max in [1u64, 2, 3, 5, 7] {
        let mut st = fresh(&p);
        let mut got = vec![];
        for _ in 0..500 {
            let r = read_new(&st, max).unwrap();
            got.extend(r.lines);
            if r.next.offset == r.next.size {
                break;
            }
            st = r.next;
        }
        assert_eq!(got, all.lines, "max {max}");
    }
}

#[test]
fn crlf_lines_lose_cr() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"uuid\":\"a\"}\r\n{\"uuid\":\"b\"}\r\n");
    let r = read_new(&fresh(&p), 1024).unwrap();
    assert_eq!(r.lines, s(&["{\"uuid\":\"a\"}", "{\"uuid\":\"b\"}"]));
}

#[test]
fn split_multibyte_char_waits() {
    let d = TempDir::new().unwrap();
    let head = b"{\"t\":\"";
    let mut content = head.to_vec();
    content.extend_from_slice("é".as_bytes());
    let p = file(&d, "a", &content);
    // budget ends between the two bytes of é (head.len()+1 bytes)
    let r1 = read_new(&fresh(&p), head.len() as u64 + 1).unwrap();
    assert!(r1.lines.is_empty());
    assert_eq!(r1.next.offset, 0);
    append(&p, b"\"}\n");
    let r2 = read_new(&r1.next, 1024).unwrap();
    assert_eq!(r2.lines, s(&["{\"t\":\"é\"}"]));
}

#[test]
fn partial_last_line_not_returned() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"a\nb");
    let r = read_new(&fresh(&p), 1024).unwrap();
    assert_eq!(r.lines, s(&["a"]));
    assert_eq!(r.next.offset, 2);
}

#[test]
fn invalid_utf8_becomes_replacement_char_and_bom_is_stripped() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"\xEF\xBB\xBF{\"uuid\":\"a\"}\nx\xFFy\n");
    let r = read_new(&fresh(&p), 1024).unwrap();
    assert_eq!(r.lines, s(&["{\"uuid\":\"a\"}", "x\u{FFFD}y"]));
    assert_eq!(r.next.first_uuid.as_deref(), Some("a"));
}

#[test]
fn line_longer_than_max_bytes_returned_once() {
    let d = TempDir::new().unwrap();
    let big = format!(
        "{{\"uuid\":\"a\",\"p\":\"{}\"}}",
        "y".repeat(10 * 1024 * 1024)
    );
    let p = file(&d, "a", format!("{big}\n{{\"uuid\":\"b\"}}\n").as_bytes());
    let r1 = read_new(&fresh(&p), 1024 * 1024).unwrap();
    assert_eq!(r1.lines[0], big);
    let r2 = read_new(&r1.next, 1024 * 1024).unwrap();
    let mut all = r1.lines.clone();
    all.extend(r2.lines);
    assert_eq!(all, vec![big, "{\"uuid\":\"b\"}".to_string()]);
}

#[test]
fn zero_max_bytes_is_invalid_input() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"uuid\":\"a\"}\n");
    let e = read_new(&fresh(&p), 0).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn unaligned_start_without_newline_drops_only_rest_of_line() {
    let d = TempDir::new().unwrap();
    let p = file(
        &d,
        "a",
        b"{\"uuid\":\"a\",\"pad\":\"0123456789\"}\npartial-no-newline",
    );
    let st = initial_state(&p, 10).unwrap();
    let r1 = read_new(&st, 4).unwrap();
    assert!(r1.lines.is_empty());
    assert_eq!(r1.next.offset, r1.next.size);
    append(&p, b"rest\nnext\n");
    let r2 = read_new(&r1.next, 4).unwrap();
    assert_eq!(r2.lines, s(&["next"]));
}

#[test]
fn replaced_detection_skipped_without_head_uuid() {
    let d = TempDir::new().unwrap();
    let p = file(&d, "a", b"{\"type\":\"x\"}\n{\"type\":\"y\"}\n");
    let r1 = read_new(&fresh(&p), 1024).unwrap();
    assert_eq!(r1.next.first_uuid, None);
    fs::write(
        &p,
        b"{\"type\":\"p\"}\n{\"type\":\"q\"}\n{\"type\":\"r\"}\n",
    )
    .unwrap();
    let r2 = read_new(&r1.next, 1024).unwrap();
    assert!(!r2.reset);
    assert_eq!(r2.lines, s(&["{\"type\":\"r\"}"]));
}

#[test]
fn offset_at_line_start_drops_no_line() {
    let d = TempDir::new().unwrap();
    let first = "{\"uuid\":\"a\"}\n";
    let p = file(
        &d,
        "a",
        format!("{first}{{\"uuid\":\"b\"}}\n{{\"uuid\":\"c\"}}\n").as_bytes(),
    );
    let mut st = fresh(&p);
    st.offset = first.len() as u64;
    st.first_uuid = Some("a".into());
    let r = read_new(&st, 1024).unwrap();
    assert_eq!(r.lines, s(&["{\"uuid\":\"b\"}", "{\"uuid\":\"c\"}"]));
}

#[test]
fn first_read_aligns_after_window() {
    let d = TempDir::new().unwrap();
    let p = d.path().join("big");
    let line = format!(
        "{}\n",
        serde_json::json!({"uuid": "u", "pad": "z".repeat(999)})
    );
    let line_len = line.len() as u64;
    let target = 25 * 1024 * 1024u64;
    let count = target.div_ceil(line_len);
    let mut f = fs::File::create(&p).unwrap();
    let block = line.repeat(1000);
    let mut written = 0;
    while written < count {
        let n = (count - written).min(1000) as usize;
        f.write_all(&block.as_bytes()[..n * line.len()]).unwrap();
        written += n as u64;
    }
    drop(f);
    let size = count * line_len;
    let st = initial_state(&p, FIRST_INDEX_BYTES).unwrap();
    assert_eq!(st.offset, size - FIRST_INDEX_BYTES);
    assert_eq!(st.size, size);
    let r = read_new(&st, 8 * 1024 * 1024).unwrap();
    // next newline at or after window start; subsequent line starts on a line boundary
    let w = size - FIRST_INDEX_BYTES;
    let first_boundary = w.div_ceil(line_len) * line_len;
    let expected_lines = (size - first_boundary) / line_len;
    let chunk_lines = r.lines.len() as u64;
    assert!(chunk_lines > 0 && chunk_lines <= expected_lines);
    assert_eq!(r.next.offset, first_boundary + chunk_lines * line_len);
    assert!(r.lines.iter().all(|l| l.len() as u64 == line_len - 1));
    assert_eq!(r.next.first_uuid.as_deref(), Some("u"));
}
