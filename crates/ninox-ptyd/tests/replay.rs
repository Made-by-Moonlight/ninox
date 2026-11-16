//! Replay recorded-style agent output through the `TerminalEngine` and
//! assert on the resulting snapshot. Fixtures are synthetic but modelled on
//! real Claude Code (Ink) and ratatui/Codex output: capability probes,
//! DEC 2026 synchronized frames that erase and redraw in place, spinners,
//! alternate screen, scroll regions, 256/truecolor SGR and wide glyphs.

mod common;

use std::time::{Duration, Instant};

use common::*;
use ninox_ptyd::{AlacrittyEngine, Color, ScreenSnapshot, TerminalEngine};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn engine() -> AlacrittyEngine {
    AlacrittyEngine::new(80, 24, 1000)
}

fn run(name: &str) -> (AlacrittyEngine, ScreenSnapshot) {
    let mut e = engine();
    e.feed(&fixture(name));
    let s = e.snapshot(100);
    (e, s)
}

fn find(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).position(|w| w == needle).expect("needle present")
}

#[test]
fn claude_startup_probes_modes_and_redraw() {
    let (mut e, s) = run("claude_startup.ansi");
    let text = s.to_plain_text();
    // The second synchronized frame replaced the first in place.
    assert!(text.contains("│ > hi"), "{text}");
    assert!(!text.contains("Try \"write a test"), "{text}");
    assert_eq!(text.matches("Welcome to Claude Code").count(), 1, "{text}");
    assert_eq!(s.scrollback_len, 0, "in-place redraw must not scroll");

    // Probes are answered (Claude Code waits for these).
    let replies = String::from_utf8(e.take_replies()).unwrap();
    assert!(replies.contains("\x1b[?2026;2$y"), "DECRQM 2026 reports supported: {replies:?}");
    assert!(replies.contains("\x1b[?6c"), "DA1: {replies:?}");
    assert!(replies.contains("\x1b]11;rgb:"), "OSC 11 background: {replies:?}");
    assert!(replies.contains("\x1b[?0u"), "kitty flags query: {replies:?}");

    assert!(s.modes.bracketed_paste);
    assert_eq!(s.modes.kitty_keyboard, 1);
    assert!(!s.cursor.visible);
    assert_eq!(s.title.as_deref(), Some("✳ Claude Code"));
    // Truecolor border.
    let border = &s.visible_lines()[0].runs[0];
    assert!(border.text.starts_with("╭──"));
    assert_eq!(border.style.fg, Color::Rgb(215, 119, 87));
}

#[test]
fn color_queries_can_be_left_to_an_attached_terminal() {
    let mut e = engine();
    e.set_answer_color_queries(false);
    e.feed(b"\x1b]11;?\x1b\\\x1b[c");
    let replies = String::from_utf8(e.take_replies()).unwrap();
    assert!(!replies.contains("]11;"));
    assert!(replies.contains("\x1b[?6c"));
}

#[test]
fn synchronized_update_is_applied_atomically() {
    let bytes = fixture("claude_startup.ansi");
    // Feed up to the middle of the second synchronized frame.
    let second_bsu = find(&bytes[20..], b"\x1b[?2026h") + 20;
    let second_bsu = second_bsu + find(&bytes[second_bsu + 1..], b"\x1b[?2026h") + 1;
    let mid = second_bsu + 200;
    let mut e = engine();
    e.feed(&bytes[..second_bsu]);
    let before = e.fingerprint();
    let before_text = e.snapshot(0).to_plain_text();
    e.feed(&bytes[second_bsu..mid]);
    assert_eq!(e.fingerprint(), before, "half a synchronized frame must not be visible");
    assert_eq!(e.snapshot(0).to_plain_text(), before_text);
    assert!(e.sync_deadline().is_some());
    e.feed(&bytes[mid..]);
    assert!(e.sync_deadline().is_none());
    assert!(e.snapshot(0).to_plain_text().contains("│ > hi"));
}

#[test]
fn unterminated_synchronized_update_flushes_on_deadline() {
    let mut e = engine();
    e.feed(b"\x1b[?2026hstuck frame");
    assert!(!e.snapshot(0).to_plain_text().contains("stuck"));
    let deadline = e.sync_deadline().expect("pending sync");
    assert!(deadline > Instant::now());
    e.flush_sync();
    assert!(e.snapshot(0).to_plain_text().contains("stuck frame"));
}

#[test]
fn spinner_redraws_in_place() {
    let (_, s) = run("spinner.ansi");
    let text = s.to_plain_text();
    assert_eq!(text.matches("Thinking…").count(), 1, "{text}");
    assert!(text.starts_with("previous output line\n✽ Thinking… (2s · esc to interrupt)\n  ⎿ tip: use /compact\n"), "{text}");
    assert_eq!(s.scrollback_len, 0);
}

#[test]
fn alt_screen_enter_and_exit() {
    let bytes = fixture("alt_screen.ansi");
    let exit = find(&bytes, b"\x1b[?1049l");
    let mut e = engine();
    e.feed(&bytes[..exit]);
    let s = e.snapshot(100);
    assert!(s.modes.alt_screen);
    assert_eq!(s.scrollback_len, 0, "alt screen has no scrollback");
    let text = s.to_plain_text();
    assert!(text.starts_with("note line 1\n"));
    assert!(!text.contains("$ less"));
    let status = s.visible_lines()[22].runs.iter().find(|r| r.text.contains("(END)")).unwrap();
    assert!(status.style.inverse);

    e.feed(&bytes[exit..]);
    let s = e.snapshot(100);
    assert!(!s.modes.alt_screen);
    assert!(s.to_plain_text().starts_with("$ less notes.txt\n$\n"), "{}", s.to_plain_text());
    assert_eq!((s.cursor.row, s.cursor.col), (1, 2));
}

#[test]
fn colors_and_attributes() {
    let (_, s) = run("colors.ansi");
    let runs = &s.visible_lines()[0].runs;
    let by_text = |t: &str| runs.iter().find(|r| r.text == t).unwrap_or_else(|| panic!("no run {t:?} in {runs:?}")).style;
    assert_eq!(by_text("red256").fg, Color::Indexed(196));
    assert_eq!(by_text("truebg").bg, Color::Rgb(10, 20, 30));
    assert_eq!(by_text("bright").fg, Color::Indexed(9));
    let f = by_text("fancy");
    assert!(f.bold && f.italic && f.underline && f.strikethrough);
    let d = by_text("diminv");
    assert!(d.dim && d.inverse);
    let m = by_text("mix");
    assert_eq!((m.fg, m.bg), (Color::Rgb(255, 128, 0), Color::Indexed(17)));

    // The ANSI repaint reproduces the styles when replayed.
    let mut e2 = engine();
    e2.feed(&s.to_ansi());
    assert_eq!(e2.snapshot(0).lines, s.visible_lines().to_vec());
}

#[test]
fn wide_chars_and_emoji() {
    let (_, s) = run("wide.ansi");
    let lines = s.visible_lines();
    assert_eq!(lines[0].runs[0].text, "日本語 text");
    assert_eq!(lines[1].runs[0].text, "🦉 ninox");
    assert_eq!(lines[2].runs[0].text, "cafe\u{301}!");
    // Wide glyphs take two columns: the cursor after "日本語" is at col 6.
    let mut e = engine();
    e.feed("日本語".as_bytes());
    assert_eq!(e.snapshot(0).cursor.col, 6);
}

#[test]
fn scroll_region_inline_viewport() {
    let (_, s) = run("scroll_region.ansi");
    let lines: Vec<String> = s.visible_lines().iter().map(|l| l.runs.iter().map(|r| r.text.as_str()).collect()).collect();
    // Ten lines scrolled through a 6-row region: the last six remain, and
    // (as in xterm) the ones scrolled out of a top-anchored region become
    // scrollback.
    assert_eq!(s.scrollback_len, 10, "one line per scroll: six blanks, then history 1-4");
    assert_eq!(&lines[..6], &["history 5", "history 6", "history 7", "history 8", "history 9", "history 10"]);
    assert_eq!(lines[7], "› type here");
    assert_eq!(lines[8], "⏎ send   ⌃C quit");
    assert_eq!((s.cursor.row, s.cursor.col), (7, 11));
    assert!(s.modes.bracketed_paste);
}

#[test]
fn chunking_does_not_change_the_result() {
    for name in ["claude_startup.ansi", "spinner.ansi", "alt_screen.ansi", "colors.ansi", "wide.ansi", "scroll_region.ansi"] {
        let bytes = fixture(name);
        let (_, whole) = run(name);
        let mut e = engine();
        for chunk in bytes.chunks(7) {
            e.feed(chunk);
        }
        assert_eq!(e.snapshot(100), whole, "{name}");
    }
}

#[test]
fn history_ansi_round_trips_through_an_emulator() {
    let mut e = AlacrittyEngine::new(40, 5, 100);
    for i in 0..20 {
        e.feed(format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 8).as_bytes());
    }
    assert_eq!(e.history_size(), 16);
    let cap = e.history_ansi(-16, -1);
    let text = String::from_utf8(cap.clone()).unwrap();
    assert_eq!(text.lines().count(), 16);
    assert!(text.starts_with("\x1b[0;30mline 0\x1b[0m\n"), "{text:?}");
    assert!(text.ends_with("line 15\x1b[0m\n"));
    // Swapped bounds behave like tmux (normalised).
    assert_eq!(e.history_ansi(-1, -16), cap);
}

/// The full pipeline (PTY → reader → engine) agrees with feeding the
/// engine directly, whatever chunking the PTY produced.
#[tokio::test]
async fn host_pipeline_matches_direct_replay() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    let path = format!("{}/tests/fixtures/claude_startup.ansi", env!("CARGO_MANIFEST_DIR"));
    let mut sp = spec("replay", &format!("stty raw -echo; cat '{path}'; sleep 30"));
    sp.cwd = env!("CARGO_MANIFEST_DIR").into();
    c.spawn(sp).await.unwrap();
    let s = wait_screen(&mut c, "replay", 0, |t| t.contains("│ > hi")).await;
    let (_, direct) = run("claude_startup.ansi");
    assert_eq!(s.visible_lines(), direct.visible_lines());
    assert_eq!(s.title, direct.title);
    assert_eq!(s.modes, direct.modes);
}

#[tokio::test]
async fn host_flushes_unterminated_sync() {
    let h = TestHost::start().await;
    let mut c = h.client().await;
    c.spawn(spec("sync", "printf '\\033[?2026hnever-ended'; sleep 30")).await.unwrap();
    let start = Instant::now();
    wait_screen(&mut c, "sync", 0, |t| t.contains("never-ended")).await;
    assert!(start.elapsed() < Duration::from_secs(2));
}

/// Rough throughput check: 15 populated panes' worth of agent output.
/// `cargo test -p ninox-ptyd --release -- --ignored --nocapture bench`
#[test]
#[ignore]
fn bench_fifteen_panes() {
    let spinner = fixture("spinner.ansi");
    let startup = fixture("claude_startup.ansi");
    let mut engines: Vec<AlacrittyEngine> = (0..15).map(|_| AlacrittyEngine::new(200, 50, 10_000)).collect();
    let mut chunk = Vec::new();
    for i in 0..200 {
        chunk.extend_from_slice(&startup);
        chunk.extend_from_slice(&spinner);
        chunk.extend_from_slice(format!("log line {i} with some ordinary agent output text\r\n").as_bytes());
    }
    let start = Instant::now();
    for e in engines.iter_mut() {
        e.feed(&chunk);
        let _ = e.take_replies();
    }
    let feed = start.elapsed();
    let start = Instant::now();
    for e in &engines {
        let _ = e.fingerprint();
        let _ = e.snapshot(0);
    }
    let present = start.elapsed();
    let mb = (chunk.len() * 15) as f64 / 1e6;
    println!("fed {mb:.1} MB into 15 panes in {feed:?} ({:.1} MB/s); fingerprint+snapshot of 15 panes: {present:?}", mb / feed.as_secs_f64());
}

/// Host-level: 15 live panes streaming output while one is watched.
#[tokio::test]
#[ignore]
async fn bench_fifteen_live_panes() {
    let h = TestHost::start_with(false).await;
    let mut c = h.client().await;
    let start = Instant::now();
    for i in 0..15 {
        let mut sp = spec(&format!("b{i}"), "i=0; while [ $i -lt 20000 ]; do echo \"agent output line $i lorem ipsum dolor sit amet\"; i=$((i+1)); done; exit 0");
        sp.cols = 200;
        sp.rows = 50;
        c.spawn(sp).await.unwrap();
    }
    let mut sub = ninox_ptyd::PtydClient::subscribe(&h.socket, "b0", ninox_ptyd::SubscribeMode::Frames).await.unwrap();
    let mut frames = 0;
    let watcher = tokio::spawn(async move {
        while let Ok(Some((ev, _))) = sub.next().await {
            frames += 1;
            if matches!(ev, ninox_ptyd::Event::Exited { .. }) {
                break;
            }
        }
        frames
    });
    wait_until("all panes exit", || async {
        let mut c = ninox_ptyd::PtydClient::connect(&h.socket, "t").await.unwrap();
        c.list().await.unwrap().iter().all(|p| !p.alive)
    })
    .await;
    let frames = watcher.await.unwrap();
    println!("15 panes x 20k lines in {:?}; watched pane got {frames} frame events", start.elapsed());
}
