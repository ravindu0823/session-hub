//! The TUI over a seeded store and ratatui's `TestBackend`: nothing here needs a terminal. The
//! `run_loop` tests drive the real loop with scripted events.
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use hub_core::platform::{FakePlatform, FocusResult, Platform};
use hub_core::status::{Status, WaitingItem};
use hub_core::store::{LiveStatus, MessageRow, Role, SessionRow, SessionView, Store};
use hub_core::view::snapshot::{DaemonState, HubSnapshot, SnapshotMeta, Totals};
use hub_tui::app::{Action, App, Tab};
use hub_tui::{Driver, Events, run_loop};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

const NOW: i64 = 1_794_650_400_000 + 600_000; // 2026-11-14T10:10:00Z
const REPLY_TS: &str = "2026-11-14T10:00:00.000Z";
const KEY3: &str = "Sending arrives in the next version of the Rust hub.";

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent {
        code: c,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}
fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn session(id: &str, name: &str, profile: &str, cwd: &str) -> SessionRow {
    SessionRow {
        session_id: id.into(),
        profile: profile.into(),
        name: name.into(),
        cwd: cwd.into(),
        pid: 4242,
        kind: "interactive".into(),
        status: LiveStatus::Idle,
        started_at: 1000,
        updated_at: 2000,
        ended_at: None,
        transcript_path: format!("/x/{id}.jsonl"),
        permission_mode: None,
    }
}

fn msg(uuid: &str, sid: &str, ts: &str, role: Role, text: &str) -> MessageRow {
    MessageRow {
        uuid: uuid.into(),
        session_id: sid.into(),
        ts: ts.into(),
        role,
        from_name: None,
        text: text.into(),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Store,
}

/// alpha (claude, with a status whose Q1 the owner answered) and beta (claude-personal).
fn seeded() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.db");
    let store = Store::open(&path, true).unwrap();
    store
        .upsert_session(&session("s1", "alpha", "claude", "/work/alpha"))
        .unwrap();
    store
        .upsert_session(&session("s2", "beta", "claude-personal", "/work/beta"))
        .unwrap();
    let status = Status {
        progress_pct: Some(40),
        phase: Some("build".into()),
        remaining: vec!["write the tests".into()],
        waiting: ["Q1", "Q2"]
            .iter()
            .map(|id| WaitingItem {
                id: Some((*id).into()),
                text: format!("question {id}"),
                is_new: false,
                children: vec![],
            })
            .collect(),
        recommendations: vec![],
    };
    store.set_status("s1", &status, "r1", REPLY_TS).unwrap();
    store
        .add_messages(
            "s1",
            &[
                msg(
                    "m1",
                    "s1",
                    "2026-11-14T09:00:00.000Z",
                    Role::Owner,
                    "please start",
                ),
                msg(
                    "m2",
                    "s1",
                    "2026-11-14T09:30:00.000Z",
                    Role::Assistant,
                    "hello from history",
                ),
                msg(
                    "m3",
                    "s1",
                    "2026-11-14T10:05:00.000Z",
                    Role::Owner,
                    "Q1 go ahead",
                ),
                msg(
                    "m4",
                    "s1",
                    "2026-11-14T10:06:00.000Z",
                    Role::Assistant,
                    "we will deploy the build tonight",
                ),
            ],
        )
        .unwrap();
    Fixture {
        _dir: dir,
        path,
        store,
    }
}

fn driver(path: &Path, platform: Arc<dyn Platform>) -> Driver {
    Driver::new(
        path,
        platform,
        &["claude", "claude-personal"],
        Box::new(|| true),
        Box::new(|| NOW),
        None,
    )
}

fn screen(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    let area = buf.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn draw(term: &mut Terminal<TestBackend>, d: &mut Driver) -> String {
    term.draw(|f| d.draw(f)).unwrap();
    screen(term)
}

fn term() -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(100, 30)).unwrap()
}

#[test]
fn render_smoke_list_status_history_search() {
    let fx = seeded();
    let mut d = driver(&fx.path, Arc::new(FakePlatform));
    let mut t = term();
    d.refresh();
    let s = draw(&mut t, &mut d);
    // The list: both profile headers, names, cwd, state.
    assert!(s.lines().any(|l| l.starts_with(".claude ")), "{s}");
    assert!(s.contains(".claude-personal"), "{s}");
    assert!(s.contains("> alpha"), "{s}");
    assert!(s.contains("beta"), "{s}");
    assert!(s.contains("/work/alpha"), "{s}");
    assert!(s.contains("idle"), "{s}");
    // The selected session's Status pane.
    assert!(!s.contains("question Q1"), "{s}");
    assert!(s.contains("Q2 question Q2"), "{s}");
    assert!(s.contains("Progress: 40% overall. Phase: build"), "{s}");
    assert!(s.contains("What's remaining"), "{s}");

    // 2: History.
    assert!(d.key(ch('2')));
    let s = draw(&mut t, &mut d);
    assert!(s.contains("hello from history"), "{s}");
    assert!(s.contains("claude"), "{s}");
    assert!(s.contains("you"), "{s}");

    // / deploy Enter: a snippet with the match in brackets.
    assert!(d.key(ch('/')));
    let s = draw(&mut t, &mut d);
    assert!(s.contains("Search: _"), "{s}");
    for c in "deploy".chars() {
        assert!(d.key(ch(c)));
    }
    assert!(draw(&mut t, &mut d).contains("Search: deploy_"));
    assert!(d.key(key(KeyCode::Enter)));
    let s = draw(&mut t, &mut d);
    assert!(s.contains("[deploy]"), "{s}");
    assert!(s.contains("1 result (Esc clears)"), "{s}");
    // Esc clears the results, and the history is back.
    assert!(d.key(key(KeyCode::Esc)));
    assert!(draw(&mut t, &mut d).contains("hello from history"));
}

#[test]
fn down_selects_the_next_session_and_up_goes_back() {
    let fx = seeded();
    let mut d = driver(&fx.path, Arc::new(FakePlatform));
    let mut t = term();
    d.refresh();
    d.key(key(KeyCode::Down));
    let s = draw(&mut t, &mut d);
    assert!(s.contains("> beta"), "{s}");
    assert!(s.contains("no status reply yet"), "{s}");
    d.key(key(KeyCode::Up));
    assert!(draw(&mut t, &mut d).contains("> alpha"));
    // Up at the top stays.
    d.key(key(KeyCode::Up));
    assert!(draw(&mut t, &mut d).contains("> alpha"));
}

#[test]
fn key_3_shows_exactly_one_line_and_1_goes_back() {
    let fx = seeded();
    let mut d = driver(&fx.path, Arc::new(FakePlatform));
    let mut t = term();
    d.refresh();
    d.key(ch('3'));
    let s = draw(&mut t, &mut d);
    assert_eq!(s.matches(KEY3).count(), 1, "{s}");
    assert!(!s.contains("Q2 question Q2"), "{s}");
    d.key(ch('1'));
    let s = draw(&mut t, &mut d);
    assert!(!s.contains(KEY3), "{s}");
    assert!(s.contains("Q2 question Q2"));
}

#[test]
fn history_pages_older_with_pgup_and_end_returns_to_the_newest() {
    let fx = seeded();
    let rows: Vec<MessageRow> = (0..120)
        .map(|i| {
            msg(
                &format!("h{i:03}"),
                "s1",
                &format!("2026-11-13T{:02}:{:02}:00.000Z", i / 60, i % 60),
                Role::Assistant,
                &format!("old line {i:03}"),
            )
        })
        .collect();
    fx.store.add_messages("s1", &rows).unwrap();
    let mut d = driver(&fx.path, Arc::new(FakePlatform));
    let mut t = term();
    d.refresh();
    d.key(ch('2'));
    let s = draw(&mut t, &mut d);
    assert!(s.contains("we will deploy the build tonight"), "{s}");
    assert!(!s.contains("old line 000"), "{s}");
    // Page up until the first loaded page is passed: older messages get loaded.
    for _ in 0..40 {
        d.key(key(KeyCode::PageUp));
    }
    let s = draw(&mut t, &mut d);
    assert!(s.contains("old line 0"), "{s}");
    assert!(!s.contains("we will deploy the build tonight"), "{s}");
    d.key(key(KeyCode::End));
    let s = draw(&mut t, &mut d);
    assert!(s.contains("we will deploy the build tonight"), "{s}");
}

// ---- R16: a store error never turns into "no sessions" ----

fn snapshot_with(names: &[&str]) -> HubSnapshot {
    let sessions: Vec<SessionView> = names
        .iter()
        .map(|n| SessionView {
            session_id: format!("id-{n}"),
            profile: "claude".into(),
            name: (*n).into(),
            cwd: "/w".into(),
            pid: 1,
            kind: "interactive".into(),
            started_at: 0,
            updated_at: 0,
            ended_at: None,
            transcript_path: "/w/x.jsonl".into(),
            permission_mode: None,
            state: LiveStatus::Idle,
            status: None,
            latest_reply: None,
            waiting_count: 0,
        })
        .collect();
    HubSnapshot {
        sessions,
        meta: SnapshotMeta {
            unknown_count: 0,
            unknown_version: None,
            stale: vec![],
            daemon: DaemonState::Running,
            relay_stray_count: 0,
            unreadable_sessions: 0,
        },
        totals: Totals {
            waiting_items: 0,
            needs_approval: 0,
            live: names.len() as i64,
        },
    }
}

fn render_app(app: &App) -> String {
    let mut t = term();
    t.draw(|f| app.render(f)).unwrap();
    screen(&t)
}

#[test]
fn an_error_keeps_the_previous_snapshot_and_shows_an_error_line() {
    let mut app = App::new(None);
    app.set_area(Rect::new(0, 0, 100, 30));
    app.on_snapshot(Ok(snapshot_with(&["alpha"])), NOW);
    assert!(render_app(&app).contains("> alpha"));
    app.on_snapshot(Err("database is locked".into()), NOW);
    let s = render_app(&app);
    assert!(s.contains("> alpha"), "{s}");
    assert!(s.contains("store error: database is locked"), "{s}");
    assert!(!s.contains("no sessions"), "{s}");
    // The next good read clears the error.
    app.on_snapshot(Ok(snapshot_with(&["alpha"])), NOW);
    assert!(!render_app(&app).contains("store error"));
}

#[test]
fn an_error_before_the_first_snapshot_is_store_unavailable_not_no_sessions() {
    let mut app = App::new(None);
    app.set_area(Rect::new(0, 0, 100, 30));
    assert!(render_app(&app).contains("loading..."));
    app.on_snapshot(Err("unable to open database file".into()), NOW);
    let s = render_app(&app);
    assert!(s.contains("store unavailable"), "{s}");
    assert!(
        s.contains("store error: unable to open database file"),
        "{s}"
    );
    assert!(!s.contains("no sessions"), "{s}");
    // An empty good snapshot is the real "no sessions".
    app.on_snapshot(Ok(snapshot_with(&[])), NOW);
    let s = render_app(&app);
    assert!(s.contains("no sessions"), "{s}");
    assert!(!s.contains("store error"), "{s}");
}

#[test]
fn error_text_is_terminal_safe() {
    let mut app = App::new(None);
    app.set_area(Rect::new(0, 0, 100, 30));
    app.on_snapshot(Err("bad \x1b[31mred\x1b]0;title\x07 end".into()), NOW);
    let s = render_app(&app);
    assert!(s.contains("store error: bad red end"), "{s}");
    assert!(!s.contains('\x1b'));
}

#[test]
fn a_missing_database_is_store_unavailable_and_recovers_when_it_appears() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.db");
    let mut d = driver(&path, Arc::new(FakePlatform));
    let mut t = term();
    d.refresh();
    let s = draw(&mut t, &mut d);
    assert!(s.contains("store unavailable"), "{s}");
    assert!(s.contains("store error:"), "{s}");
    assert!(!s.contains("no sessions"), "{s}");
    // The daemon creates it a moment later.
    let store = Store::open(&path, true).unwrap();
    store
        .upsert_session(&session("s1", "alpha", "claude", "/w"))
        .unwrap();
    d.refresh();
    let s = draw(&mut t, &mut d);
    assert!(s.contains("> alpha"), "{s}");
    assert!(!s.contains("store error"), "{s}");
}

#[test]
fn a_database_from_a_newer_build_is_refused_and_shown_as_an_error() {
    let fx = seeded();
    rusqlite::Connection::open(&fx.path)
        .unwrap()
        .execute("INSERT INTO schema_version (version) VALUES (99)", [])
        .unwrap();
    let mut d = driver(&fx.path, Arc::new(FakePlatform));
    let mut t = term();
    d.refresh();
    let s = draw(&mut t, &mut d);
    assert!(s.contains("newer than this build supports"), "{s}");
    assert!(!s.contains("no sessions"), "{s}");
}

// ---- focus (j) ----

struct FocusPlatform(FocusResult);

impl Platform for FocusPlatform {
    fn ensure_private_dir(&self, d: &Path) -> io::Result<()> {
        FakePlatform.ensure_private_dir(d)
    }
    fn is_private_dir(&self, d: &Path) -> io::Result<bool> {
        FakePlatform.is_private_dir(d)
    }
    fn process_alive(&self, pid: u32) -> bool {
        FakePlatform.process_alive(pid)
    }
    fn process_start_time(&self, pid: u32) -> Option<i64> {
        FakePlatform.process_start_time(pid)
    }
    fn process_image_path(&self, pid: u32) -> Option<PathBuf> {
        FakePlatform.process_image_path(pid)
    }
    fn spawn_hidden(&self, cmd: &mut std::process::Command) {
        FakePlatform.spawn_hidden(cmd)
    }
    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        FakePlatform.kill_tree(pid)
    }
    fn focus_session_window(&self, _pid: u32) -> FocusResult {
        self.0.clone()
    }
}

fn focus_toast(result: FocusResult) -> String {
    let fx = seeded();
    let mut d = driver(&fx.path, Arc::new(FocusPlatform(result)));
    let mut t = term();
    d.refresh();
    d.key(ch('j'));
    draw(&mut t, &mut d)
}

#[test]
fn j_toasts_the_focused_title_the_reason_or_unsupported() {
    assert!(
        focus_toast(FocusResult::Focused {
            title: "my terminal".into()
        })
        .contains("focused: my terminal")
    );
    assert!(
        focus_toast(FocusResult::NotFound {
            reason: "no-window".into()
        })
        .contains("could not focus: no-window")
    );
    assert!(
        focus_toast(FocusResult::Unsupported)
            .contains("could not focus: not supported on this OS yet")
    );
    // A window title is store-independent text: escapes never reach the terminal.
    let s = focus_toast(FocusResult::Focused {
        title: "a\x1b[2Jb".into(),
    });
    assert!(s.contains("focused: ab") && !s.contains('\x1b'), "{s}");
}

#[test]
fn a_toast_lasts_3_seconds() {
    let mut app = App::new(None);
    app.set_area(Rect::new(0, 0, 100, 30));
    app.on_snapshot(Ok(snapshot_with(&["alpha"])), NOW);
    let t0 = Instant::now();
    app.show_toast("hello toast", t0);
    assert!(render_app(&app).contains("hello toast"));
    app.expire_toast(t0 + Duration::from_millis(2900));
    assert!(render_app(&app).contains("hello toast"));
    app.expire_toast(t0 + Duration::from_millis(3000));
    let s = render_app(&app);
    assert!(!s.contains("hello toast"));
    assert!(s.contains("j jump"), "{s}");
}

#[test]
fn j_on_an_ended_session_does_not_try_to_focus() {
    let mut app = App::new(None);
    app.set_area(Rect::new(0, 0, 100, 30));
    let mut snap = snapshot_with(&["alpha"]);
    snap.sessions[0].ended_at = Some(5);
    app.on_snapshot(Ok(snap), NOW);
    assert_eq!(app.handle_key(ch('j')), Action::None);
    assert!(render_app(&app).contains("could not focus: session ended"));
}

// ---- keys at the state level ----

#[test]
fn handle_key_actions() {
    let mut app = App::new(None);
    app.set_area(Rect::new(0, 0, 100, 30));
    app.on_snapshot(Ok(snapshot_with(&["alpha", "beta"])), NOW);
    assert_eq!(app.handle_key(ch('q')), Action::Quit);
    assert_eq!(
        app.handle_key(KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..ch('c')
        }),
        Action::Quit
    );
    assert_eq!(app.handle_key(ch('r')), Action::Refresh);
    assert_eq!(app.handle_key(ch('j')), Action::Focus(1));
    assert_eq!(app.handle_key(ch('x')), Action::None);
    assert_eq!(app.tab(), Tab::Status);
    app.handle_key(ch('2'));
    assert_eq!(app.tab(), Tab::History);
    app.handle_key(ch('3'));
    assert_eq!(app.tab(), Tab::Send);
    app.handle_key(ch('/'));
    assert_eq!(app.tab(), Tab::History);
    // While typing a search, q and digits are text, not commands.
    assert_eq!(app.handle_key(ch('q')), Action::None);
    app.handle_key(ch('2'));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Action::Search("q2".into())
    );
}

#[test]
fn control_characters_never_enter_the_search_query() {
    let mut app = App::new(None);
    app.set_area(Rect::new(0, 0, 100, 30));
    app.on_snapshot(Ok(snapshot_with(&["alpha"])), NOW);
    app.handle_key(ch('/'));
    app.paste("a\x1b[31mb\x1b]0;x\x07c\nd");
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Action::Search("abc d".into())
    );
    // Enter leaves the box open until the driver hands the results back; Esc cancels it.
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(ch('/'));
    app.handle_key(ch('a'));
    app.handle_key(ch('b'));
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Action::Search("a".into())
    );
    // Esc cancels the input.
    app.handle_key(ch('/'));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.handle_key(ch('q')), Action::Quit);
}

// ---- the real loop with scripted events ----

struct Script {
    events: VecDeque<Event>,
    polls: usize,
}

impl Events for Script {
    fn poll(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        self.polls += 1;
        Ok(Some(self.events.pop_front().unwrap_or(Event::Key(ch('q')))))
    }
}

#[test]
fn run_loop_draws_handles_events_and_quits() {
    let fx = seeded();
    let mut d = driver(&fx.path, Arc::new(FakePlatform));
    let mut t = term();
    let mut ev = Script {
        events: [
            Event::Key(ch('2')),
            Event::Resize(100, 30),
            Event::Key(ch('/')),
        ]
        .into_iter()
        .chain("deploy".chars().map(|c| Event::Key(ch(c))))
        .chain([Event::Key(key(KeyCode::Enter))])
        .collect(),
        polls: 0,
    };
    // The script ends with `q` while search results are showing: it quits (results do not capture).
    run_loop(&mut t, &mut d, &mut ev, Duration::from_secs(1)).unwrap();
    assert!(ev.polls >= 11);
    assert!(screen(&t).contains("[deploy]"), "{}", screen(&t));
}

#[test]
fn key_release_events_are_ignored() {
    let fx = seeded();
    let mut d = driver(&fx.path, Arc::new(FakePlatform));
    let mut t = term();
    let release = KeyEvent {
        kind: KeyEventKind::Release,
        ..ch('2')
    };
    let mut ev = Script {
        events: [Event::Key(release)].into(),
        polls: 0,
    };
    run_loop(&mut t, &mut d, &mut ev, Duration::from_secs(1)).unwrap();
    assert!(screen(&t).contains("Q2 question Q2"), "still on Status");
}

#[test]
fn the_warning_stays_on_screen() {
    let fx = seeded();
    let mut d = Driver::new(
        &fx.path,
        Arc::new(FakePlatform),
        &["claude"],
        Box::new(|| false),
        Box::new(|| NOW),
        Some(
            "The daemon is not running and did not start; the list may be stale. See /x/daemon.log"
                .into(),
        ),
    );
    let mut t = term();
    d.refresh();
    let s = draw(&mut t, &mut d);
    assert!(
        s.contains("! The daemon is not running and did not start"),
        "{s}"
    );
}
