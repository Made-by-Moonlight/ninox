//! The inspector beside an attached or zoomed pane (`i`): an Info tab (the
//! session's facts, PR, CI, full review comments, Definition of done,
//! Outcome) and a Diff tab (a file list over the workspace's diff; Enter
//! opens one file's unified diff). Lines are pre-wrapped to the panel's
//! width so `layout` can clamp scrolling to their exact count.

use ninox_overview::diff::FileDiff;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::palette::Palette;
use super::state::{Row, TuiState};

/// How often a showing Diff tab refetches — a live session keeps committing.
/// Arbitrary; bump if `git diff` on big worktrees makes it feel busy.
pub const DIFF_REFRESH_MS: i64 = 10_000;

/// Rows above the content: the tab strip.
pub const HEAD_H: u16 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Info,
    Diff,
}

/// The Diff tab's fetched diff and cursor, for one session at a time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiffView {
    pub for_id: Option<String>,
    pub loading: bool,
    pub fetched_ms: i64,
    pub files: Vec<FileDiff>,
    pub selected: usize,
    /// The file whose unified diff is open, if any.
    pub open: Option<usize>,
    pub scroll: u16,
    /// Why the last fetch has nothing to list, when it failed outright.
    pub error: Option<String>,
    /// The open file's wrapped lines, keyed by what they were built from —
    /// `layout` and `draw_inspector` both need them every frame, and a long
    /// file's diff is too much to re-wrap each time.
    #[allow(clippy::type_complexity)]
    pub wrapped: std::cell::RefCell<Option<((usize, usize, i64, Palette), Vec<Line<'static>>)>>,
}

impl DiffView {
    pub fn due(&self, id: &str, now_ms: i64) -> bool {
        !self.loading && (self.for_id.as_deref() != Some(id) || now_ms - self.fetched_ms >= DIFF_REFRESH_MS)
    }

    /// A fetch for `id` is starting; a different session's cursor resets.
    pub fn begin(&mut self, id: &str) {
        if self.for_id.as_deref() != Some(id) {
            *self = DiffView { for_id: Some(id.to_string()), ..Default::default() };
        }
        self.loading = true;
    }

    /// A fetch finished (`Err`: it couldn't run at all); dropped if the
    /// selection moved on meanwhile.
    pub fn finish(&mut self, id: &str, diff: Result<Option<&str>, &str>, now_ms: i64) {
        if self.for_id.as_deref() != Some(id) {
            return;
        }
        self.loading = false;
        self.fetched_ms = now_ms;
        self.error = diff.err().map(str::to_string);
        self.files = diff.ok().flatten().map(ninox_overview::diff::files).unwrap_or_default();
        self.wrapped.replace(None);
        self.selected = self.selected.min(self.files.len().saturating_sub(1));
        if self.open.is_some_and(|i| i >= self.files.len()) {
            self.open = None;
            self.scroll = 0;
        }
    }
}

/// The content area under the tab strip, inside the left rule and padding.
pub fn body(area: Rect) -> Rect {
    let x = area.x + 2;
    Rect { x, y: area.y + HEAD_H, width: area.width.saturating_sub(3), height: area.height.saturating_sub(HEAD_H) }
}

/// The scrolled content for the current tab. The Diff file list scrolls by
/// its cursor instead, so it isn't counted here.
pub fn lines(st: &TuiState, width: usize) -> Vec<Line<'static>> {
    let Some(row) = st.selected_row() else { return Vec::new() };
    match st.inspector_tab {
        Tab::Info => info_lines(st, row, width),
        Tab::Diff => diff_lines(st, row, width),
    }
}

fn heading(p: &Palette, text: String) -> Line<'static> {
    Line::from(Span::styled(text, Style::default().fg(p.faint).add_modifier(Modifier::BOLD)))
}

/// `PULL REQUEST`, with the count once there is more than one.
pub(super) fn pr_heading(count: usize) -> String {
    if count > 1 { format!("PULL REQUEST · {count}") } else { "PULL REQUEST".to_string() }
}

/// One entry per PR, open first (the order `session_prs` returns): a state
/// glyph and `repo#n title`, then an indented state and checks line.
pub(super) fn pr_entries(prs: &[ninox_core::session_prs::SessionPr]) -> Vec<(String, String)> {
    use ninox_core::session_prs::PrState;
    prs.iter()
        .map(|pr| {
            let glyph = match (pr.state, pr.draft) {
                (PrState::Merged, _) => "✓",
                (PrState::Closed, _) | (PrState::Open, true) => "○",
                (PrState::Open, false) => "●",
            };
            let title = if pr.title.is_empty() { pr.label() } else { format!("{} {}", pr.label(), pr.title) };
            let mut detail = pr.state_line();
            if pr.is_open() {
                if let Some(checks) = pr.checks_line() {
                    detail = format!("{detail} · {checks}");
                }
            }
            (format!("{glyph} {title}"), detail)
        })
        .collect()
}

fn pr_tone(p: &Palette, pr: &ninox_core::session_prs::SessionPr) -> ratatui::style::Color {
    use ninox_core::session_prs::PrState;
    match pr.state {
        PrState::Merged => p.done,
        PrState::Closed => p.faint,
        PrState::Open if pr.failing > 0 || pr.changes_requested => p.ci_failed,
        PrState::Open if pr.pending > 0 => p.review,
        PrState::Open if pr.draft => p.faint,
        PrState::Open => p.working,
    }
}

fn push_pr_list(out: &mut Vec<Line<'static>>, p: &Palette, prs: &[ninox_core::session_prs::SessionPr], width: usize) {
    for (pr, (title, detail)) in prs.iter().zip(pr_entries(prs)) {
        wrapped(out, &title, width, Style::default().fg(p.ink));
        for l in wrap(&detail, width.saturating_sub(2)) {
            out.push(Line::from(Span::styled(format!("  {l}"), Style::default().fg(pr_tone(p, pr)))));
        }
    }
}

/// Word wrap that keeps blank lines and splits a word longer than `width`
/// (a path, a URL) across lines rather than clipping it.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            if !cur.is_empty() && cur.width() + 1 + word.width() > width {
                out.push(std::mem::take(&mut cur));
            }
            if word.width() > width {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                let mut pieces = wrap_chars(word, width);
                cur = pieces.pop().unwrap_or_default();
                out.extend(pieces);
                continue;
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        out.push(cur);
    }
    out
}

fn wrapped(out: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for l in wrap(text, width) {
        out.push(Line::from(Span::styled(l, style)));
    }
}

const KEY_W: usize = 10;

fn kv(out: &mut Vec<Line<'static>>, p: &Palette, k: &str, v: String, width: usize) {
    let mut first = true;
    for l in wrap(&v, width.saturating_sub(KEY_W)) {
        let key = if first { format!("{:<KEY_W$}", k.to_uppercase()) } else { " ".repeat(KEY_W) };
        first = false;
        out.push(Line::from(vec![Span::styled(key, Style::default().fg(p.faint)), Span::styled(l, Style::default().fg(p.ink))]));
    }
}

pub fn info_lines(st: &TuiState, r: &Row, width: usize) -> Vec<Line<'static>> {
    use super::view::{activity_label, ago_opt};
    let p = &st.palette;
    let s = &r.session;
    let mut out = Vec::new();
    kv(&mut out, p, "id", s.id.clone(), width);
    kv(&mut out, p, "name", s.name.clone(), width);
    let role = if r.is_orchestrator {
        "orchestrator".to_string()
    } else {
        r.group.as_ref().map(|g| format!("worker of {g}")).unwrap_or_else(|| "standalone".into())
    };
    kv(&mut out, p, "role", role, width);
    kv(&mut out, p, "status", r.status().to_string(), width);
    let since = s.activity_since.and_then(|t| ago_opt(st.now_ms, t)).map(|a| format!(" for {a}")).unwrap_or_default();
    kv(&mut out, p, "activity", format!("{}{since}", activity_label(s.activity)), width);
    if let Some(n) = &s.activity_note {
        kv(&mut out, p, "note", n.clone(), width);
    }
    kv(&mut out, p, "repo", s.repo.clone(), width);
    if let Some(w) = &s.workspace_path {
        kv(&mut out, p, "workspace", w.clone(), width);
    }
    if let Some(g) = &s.gate_status {
        kv(&mut out, p, "gate", format!("ci {:?} · review {:?} · merge {:?}", g.ci, g.review, g.mergeable).to_lowercase(), width);
    }
    kv(&mut out, p, "cost", format!("${:.2}", s.cost_usd), width);
    if r.is_orchestrator {
        // Always shown for an orchestrator, `—` until it reports.
        let meter = ninox_ui_shared::context_meter::ContextMeter::for_session(s, &st.reset_cfg);
        kv(&mut out, p, "context", meter.sheet_line(), width);
    } else if let Some(pct) = s.context_used_pct {
        kv(&mut out, p, "context", format!("{pct:.0}%"), width);
    }
    kv(&mut out, p, "harness", format!("{}{}", s.agent_type, s.model.as_ref().map(|m| format!(" · {m}")).unwrap_or_default()), width);
    if let Some(line) = st.brain_lines.get(&s.id) {
        kv(&mut out, p, "brains", line.trim_start_matches("Brains: ").to_string(), width);
    }
    if s.started_at > 0 {
        kv(&mut out, p, "started", ago_opt(st.now_ms, s.started_at).map_or_else(|| "—".into(), |a| format!("{a} ago")), width);
    }
    let backend = match st.panes.get(r.id()) {
        Some(pi) => format!(
            "ptyd · pid {} · {}x{}{}",
            pi.pid,
            pi.cols,
            pi.rows,
            if pi.alive { String::new() } else { format!(" · exited {:?}", pi.exit_code) }
        ),
        None => "tmux".into(),
    };
    kv(&mut out, p, "backend", backend, width);
    if let Some(t) = st.panes.get(r.id()).and_then(|pi| pi.title.clone()) {
        kv(&mut out, p, "title", t, width);
    }
    let unread = st.unread(r.id());
    if unread > 0 {
        kv(&mut out, p, "unread", format!("{unread} messages"), width);
    }
    for (key, value) in st.held_lines(r.id()) {
        kv(&mut out, p, key, value, width);
    }
    if let Some(sum) = &s.summary {
        out.push(Line::from(""));
        wrapped(&mut out, sum, width, Style::default().fg(p.ink_2));
    }

    // `sync_detail_pr` runs once per refresh tick, so a selection change can
    // land before it catches up — only show PR state that is this row's.
    let detail = st.fleet.detail_pr.as_ref().filter(|d| s.pr_id == Some(d.pr_id));
    if !r.prs.is_empty() {
        out.push(Line::from(""));
        out.push(heading(p, pr_heading(r.prs.len())));
        push_pr_list(&mut out, p, &r.prs, width);
    } else if s.pr_number.is_some() || detail.is_some() {
        out.push(Line::from(""));
        out.push(heading(p, "PULL REQUEST".into()));
        match detail.and_then(|d| d.pr.as_ref()) {
            Some(pr) => wrapped(&mut out, &format!("#{} {}", pr.number, pr.title), width, Style::default().fg(p.ink)),
            None => wrapped(&mut out, &format!("#{}", s.pr_number.unwrap_or_default()), width, Style::default().fg(p.ink)),
        }
        if let Some(ci) = detail.and_then(|d| d.ci.as_ref()) {
            let color = if ci.failing > 0 { p.ci_failed } else if ci.pending > 0 { p.review } else { p.working };
            let text = format!("{}/{} checks passing · {} failing · {} pending", ci.passing, ci.total, ci.failing, ci.pending);
            wrapped(&mut out, &text, width, Style::default().fg(color));
        }
    }
    if let Some(d) = detail.filter(|d| !d.comments.is_empty()) {
        out.push(Line::from(""));
        out.push(heading(p, format!("REVIEW COMMENTS · {}", d.comments.len())));
        for c in &d.comments {
            let at = match (&c.path, c.line) {
                (Some(path), Some(line)) => format!(" {path}:{line}"),
                (Some(path), None) => format!(" {path}"),
                _ => String::new(),
            };
            out.push(Line::from(vec![
                Span::styled(c.author.clone(), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
                Span::styled(super::view::truncate(&at, width.saturating_sub(c.author.width())), Style::default().fg(p.faint)),
            ]));
            wrapped(&mut out, &ninox_overview::comment_markdown::plain_text(&c.body), width, Style::default().fg(p.ink_2));
        }
    }
    let dod = ninox_overview::completion::definition_of_done(s);
    if !dod.is_empty() {
        out.push(Line::from(""));
        out.push(heading(p, "DEFINITION OF DONE".into()));
        for (done, label) in dod {
            let (mark, color) = if done { ("✓ ", p.working) } else { ("○ ", p.faint) };
            for (i, l) in wrap(&label, width.saturating_sub(2)).into_iter().enumerate() {
                let lead = if i == 0 { Span::styled(mark, Style::default().fg(color)) } else { Span::raw("  ") };
                out.push(Line::from(vec![lead, Span::styled(l, Style::default().fg(p.ink))]));
            }
        }
    }
    if let Some(outcome) = &s.completion_outcome {
        out.push(Line::from(""));
        out.push(heading(p, "OUTCOME".into()));
        for (i, l) in ninox_overview::completion::outcome_lines(outcome).into_iter().enumerate() {
            let style = if i == 0 { Style::default().fg(p.ink) } else { Style::default().fg(p.ink_2) };
            wrapped(&mut out, &l, width, style);
        }
    }
    out
}

/// Hard-wraps by display width, keeping whitespace — diff lines are code.
fn wrap_chars(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![String::new()];
    let mut used = 0;
    for c in line.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            out.push(String::new());
            used = 0;
        }
        out.last_mut().expect("never empty").push(c);
        used += w;
    }
    out
}

fn diff_line_style(p: &Palette, line: &str) -> Style {
    if line.starts_with("diff --git") || line.starts_with("+++") || line.starts_with("---") || line.starts_with("index ") {
        Style::default().fg(p.faint)
    } else if line.starts_with("@@") {
        Style::default().fg(p.accent)
    } else if line.starts_with('+') {
        Style::default().fg(p.working)
    } else if line.starts_with('-') {
        Style::default().fg(p.ci_failed)
    } else {
        Style::default().fg(p.ink_2)
    }
}

fn counts(f: &FileDiff) -> String {
    if f.binary { "binary".into() } else { format!("+{} −{}", f.added, f.removed) }
}

fn open_file_lines(p: &Palette, f: &FileDiff, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(vec![
        Span::styled(super::view::truncate(&f.path, width.saturating_sub(counts(f).width() + 1)), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(counts(f), Style::default().fg(p.faint)),
    ])];
    for line in f.text.lines() {
        let style = diff_line_style(p, line);
        out.extend(wrap_chars(line, width).into_iter().map(|l| Line::from(Span::styled(l, style))));
    }
    out
}

/// Why there's no file list to show, when there isn't one.
pub fn diff_unavailable(st: &TuiState, r: &Row) -> Option<&'static str> {
    if r.session.machine_id.is_some() {
        Some("diffs aren't available for remote sessions yet")
    } else if r.session.workspace_path.is_none() {
        Some("no recorded workspace")
    } else if st.diff.for_id.as_deref() != Some(r.id()) || (st.diff.loading && st.diff.fetched_ms == 0) {
        Some("loading diff…")
    } else if st.diff.error.is_some() {
        None
    } else if st.diff.files.is_empty() {
        Some("no committed changes against the default branch yet (uncommitted edits aren't included)")
    } else {
        None
    }
}

pub fn diff_lines(st: &TuiState, r: &Row, width: usize) -> Vec<Line<'static>> {
    let p = &st.palette;
    if let Some(why) = diff_unavailable(st, r) {
        let mut out = Vec::new();
        wrapped(&mut out, why, width, Style::default().fg(p.faint));
        return out;
    }
    let d = &st.diff;
    if let Some(e) = &d.error {
        let mut out = Vec::new();
        wrapped(&mut out, &format!("couldn't read the diff: {e}"), width, Style::default().fg(p.ci_failed));
        return out;
    }
    if let Some(i) = d.open.filter(|&i| i < d.files.len()) {
        let key = (width, i, d.fetched_ms, *p);
        if let Some((k, lines)) = d.wrapped.borrow().as_ref() {
            if *k == key {
                return lines.clone();
            }
        }
        let lines = open_file_lines(p, &d.files[i], width);
        d.wrapped.replace(Some((key, lines.clone())));
        return lines;
    }
    let (added, removed) = d.files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let n = d.files.len();
    let mut out = vec![Line::from(Span::styled(format!("{n} file{} · +{added} −{removed}", if n == 1 { "" } else { "s" }), Style::default().fg(p.faint)))];
    for (i, f) in d.files.iter().enumerate() {
        let sel = i == d.selected;
        let c = counts(f);
        let path_w = width.saturating_sub(c.width() + 3);
        let path = super::view::truncate(&f.path, path_w);
        let pad = " ".repeat(path_w.saturating_sub(path.width()) + 1);
        let bg = if sel { Style::default().bg(p.highlight) } else { Style::default() };
        let (add_c, del_c) = (p.working, p.ci_failed);
        let mut spans = vec![
            Span::styled(if sel { "▸ " } else { "  " }, bg.fg(p.accent)),
            Span::styled(path, bg.fg(p.ink)),
            Span::styled(pad, bg),
        ];
        if f.binary {
            spans.push(Span::styled(c, bg.fg(p.ink_2)));
        } else {
            spans.push(Span::styled(format!("+{}", f.added), bg.fg(add_c)));
            spans.push(Span::styled(format!(" −{}", f.removed), bg.fg(del_c)));
        }
        out.push(Line::from(spans));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_start_reads_as_a_dash_not_dash_ago() {
        let mut st = TuiState { rows: vec![Row::test_row("a")], now_ms: 1_793_302_252_000, ..Default::default() };
        st.rows[0].session.started_at = 1_793_302_252;
        st.rows[0].session.activity_since = Some(1_793_302_252);
        let text: String = info_lines(&st, &st.rows[0], 60).iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n").collect();
        assert!(!text.contains("— ago") && !text.contains("for —"), "{text}");
        assert!(text.contains("STARTED   —"), "{text}");
    }

    fn sheet(st: &TuiState, i: usize) -> String {
        info_lines(st, &st.rows[i], 80).iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n").collect()
    }

    #[test]
    fn an_orchestrators_sheet_shows_context_with_the_threshold_and_a_dash_when_unknown() {
        use ninox_core::config::{OrchestratorResetConfig, ResetMode};
        let mut st = TuiState { rows: vec![Row::test_row("o"), Row::test_row("w")], ..Default::default() };
        st.rows[0].is_orchestrator = true;
        st.reset_cfg = OrchestratorResetConfig { auto_at_context_pct: 40, mode: ResetMode::Auto };
        assert!(sheet(&st, 0).contains("CONTEXT   —"), "{}", sheet(&st, 0));
        st.rows[0].session.context_used_pct = Some(44.0);
        st.rows[0].session.context_total_tokens = Some(440_000);
        st.rows[0].session.context_window_size = Some(1_000_000);
        assert!(sheet(&st, 0).contains("44% (440k / 1000k) · reset at 40%"), "{}", sheet(&st, 0));
        st.reset_cfg.mode = ResetMode::Off;
        assert!(!sheet(&st, 0).contains("reset at"), "no threshold while off");
        assert!(!sheet(&st, 1).contains("CONTEXT"), "a worker with no reading shows no line");
    }

    #[test]
    fn wrap_chars_hard_wraps_by_display_width_keeping_indentation() {
        assert_eq!(wrap_chars("    let x = 1;", 6), vec!["    le", "t x = ", "1;"]);
        assert_eq!(wrap_chars("日本語", 4), vec!["日本", "語"]);
        assert_eq!(wrap_chars("", 4), vec![""]);
    }

    #[test]
    fn a_diff_for_another_session_resets_the_cursor_and_a_stale_result_is_dropped() {
        let mut d = DiffView::default();
        d.begin("a");
        d.finish("a", Ok(Some("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n")), 5);
        d.open = Some(0);
        assert!(!d.due("a", 6) && d.due("a", 5 + DIFF_REFRESH_MS) && d.due("b", 6));
        d.begin("b");
        assert_eq!((d.files.len(), d.open), (0, None));
        d.finish("a", Ok(Some("diff --git a/y b/y\n")), 7);
        assert!(d.loading && d.files.is_empty(), "a's late result must not land on b");
    }

    #[test]
    fn wrap_splits_an_overlong_word_instead_of_clipping_it_and_keeps_blank_lines() {
        let long = "/Users/someone/proj/ninox/.claude/worktrees/v1-x";
        let out = wrap(&format!("at {long} now\n\nnext"), 12);
        assert!(out.iter().all(|l| l.width() <= 12), "{out:?}");
        assert_eq!(out.concat().replace(' ', ""), format!("at{long}nownext"), "nothing is cut");
        assert!(out.contains(&String::new()), "the paragraph break survives: {out:?}");
    }

    #[test]
    fn a_failed_fetch_is_an_error_not_an_empty_diff() {
        let mut d = DiffView::default();
        d.begin("a");
        d.finish("a", Err("/gone no longer exists"), 1);
        assert_eq!(d.error.as_deref(), Some("/gone no longer exists"));
        d.begin("a");
        d.finish("a", Ok(None), 2);
        assert_eq!(d.error, None);
    }

    #[test]
    fn a_refetch_with_fewer_files_keeps_the_cursor_in_range() {
        let two = "diff --git a/x b/x\ndiff --git a/y b/y\n";
        let mut d = DiffView::default();
        d.begin("a");
        d.finish("a", Ok(Some(two)), 1);
        d.selected = 1;
        d.open = Some(1);
        d.begin("a");
        d.finish("a", Ok(Some("diff --git a/x b/x\n")), 2);
        assert_eq!((d.selected, d.open), (0, None));
    }
}
