//! The session report shown in the pane area for a session with no live
//! process: what it was asked to do, how it ended, its PR, the commits and
//! diff in its workspace, and its last screen.
//!
//! Gathering (`load`) runs git and store reads off the UI loop; `plan` is
//! the pure line layout shared by drawing and mouse hit-testing.

use std::path::Path;

use ninox_core::types::{GateCheck, Session, SessionStatus};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::palette::Palette;
use super::state::{Backend, Row, TuiState};

/// Most commits listed; the rest are counted.
pub const MAX_COMMITS: usize = 15;
const MAX_FILES: usize = 8;
const MAX_SCREEN_LINES: usize = 15;
const MAX_TASK_LINES: usize = 4;
/// Title, subtitle, a gap, the buttons, a gap: pinned above the scrolling body.
pub const HEAD_H: u16 = 5;
pub const BUTTON_ROW: u16 = 3;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileStat {
    pub path: String,
    pub added: u64,
    pub removed: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitSummary {
    pub branch: Option<String>,
    /// The ref commits are counted against (`origin/main`, `master`, …).
    pub base: Option<String>,
    pub commits: Vec<Commit>,
    pub commits_total: usize,
    pub files: Vec<FileStat>,
    pub uncommitted: usize,
}

impl GitSummary {
    pub fn insertions(&self) -> u64 {
        self.files.iter().map(|f| f.added).sum()
    }
    pub fn deletions(&self) -> u64 {
        self.files.iter().map(|f| f.removed).sum()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum GitState {
    #[default]
    NoWorkspace,
    Missing(String),
    NotRepo,
    Ready(GitSummary),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CiCounts {
    pub total: u32,
    pub passing: u32,
    pub failing: u32,
    pub pending: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrFacts {
    pub number: u64,
    pub title: Option<String>,
    pub url: Option<String>,
    pub ci: Option<CiCounts>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReportData {
    pub task: Option<String>,
    pub interrupt_cause: Option<String>,
    pub interrupted_at: Option<i64>,
    pub pr: Option<PrFacts>,
    pub git: GitState,
    pub resumable: bool,
}

/// A cached report; `loading` while a fresh one is being gathered (the
/// previous one, if any, stays on screen meanwhile).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReportSlot {
    pub data: Option<ReportData>,
    pub loading: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportButton {
    Remove,
    Resume,
    OpenPr,
}

impl ReportButton {
    pub fn key(self) -> &'static str {
        match self {
            Self::Remove => "x",
            Self::Resume => "r",
            Self::OpenPr => "O",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Remove => "Remove",
            Self::Resume => "Resume",
            Self::OpenPr => "Open PR",
        }
    }
    /// `[ x Remove ]`
    pub fn text(self) -> String {
        format!("[ {} {} ]", self.key(), self.label())
    }
}

// ── gathering ───────────────────────────────────────────────────────────────

/// Gathers everything the report shows. Blocking (git, sqlite).
pub fn load(store: &ninox_core::store::Store, session: &Session, is_orchestrator: bool) -> ReportData {
    let config = ninox_core::config::AppConfig::load().unwrap_or_default();
    let fleet = store.fleet_record(&session.id).ok().flatten();
    let pr = session.pr_number.map(|number| {
        let row = session.pr_id.and_then(|id| store.get_pr(id).ok().flatten());
        let ci = session.pr_id.and_then(|id| store.get_ci_status(id).ok().flatten()).map(|c| CiCounts {
            total: c.total,
            passing: c.passing,
            failing: c.failing,
            pending: c.pending,
        });
        let url = row.as_ref().map(|p| p.url.clone()).filter(|u| !u.is_empty()).or_else(|| github_pr_url(&session.repo, number));
        PrFacts { number, title: row.map(|p| p.title).filter(|t| !t.is_empty()), url, ci }
    });
    let git = match session.workspace_path.as_deref() {
        None => GitState::NoWorkspace,
        Some(ws) => collect_git(Path::new(ws)),
    };
    ReportData {
        task: fleet.as_ref().and_then(|f| f.task_brief.clone()).filter(|t| !t.trim().is_empty()),
        interrupt_cause: fleet.as_ref().and_then(|f| f.interrupt_cause.clone()),
        interrupted_at: fleet.as_ref().and_then(|f| f.interrupted_at),
        pr,
        git,
        resumable: crate::app::resume_plan(session, is_orchestrator, &config).is_some(),
    }
}

/// `owner/name` → its PR page; `None` for anything that is not a slug.
pub(super) fn github_pr_url(repo: &str, number: u64) -> Option<String> {
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(n), None) if !o.is_empty() && !n.is_empty() => Some(format!("https://github.com/{o}/{n}/pull/{number}")),
        _ => None,
    }
}

fn git(ws: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn collect_git(ws: &Path) -> GitState {
    if !ws.exists() {
        return GitState::Missing(ws.display().to_string());
    }
    if git(ws, &["rev-parse", "--is-inside-work-tree"]).as_deref().map(str::trim) != Some("true") {
        return GitState::NotRepo;
    }
    let branch = git(ws, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|b| b.trim().to_string()).filter(|b| !b.is_empty() && b != "HEAD");
    let origin_head = git(ws, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]).map(|s| s.trim().to_string());
    let candidates = origin_head.into_iter().chain(["origin/main", "origin/master", "main", "master"].map(String::from));
    let base = candidates
        .filter(|c| branch.as_deref() != Some(c.as_str()))
        .find(|c| git(ws, &["rev-parse", "--verify", "--quiet", &format!("{c}^{{commit}}")]).is_some());
    let merge_base = base.as_deref().and_then(|b| git(ws, &["merge-base", b, "HEAD"])).map(|s| s.trim().to_string());
    let mut summary = GitSummary { branch, base, ..Default::default() };
    if let Some(mb) = merge_base {
        let range = format!("{mb}..HEAD");
        let n = MAX_COMMITS.to_string();
        summary.commits = parse_log(&git(ws, &["log", "--no-decorate", "--format=%h%x09%s", "-n", &n, &range]).unwrap_or_default());
        summary.commits_total =
            git(ws, &["rev-list", "--count", &range]).and_then(|s| s.trim().parse().ok()).unwrap_or(summary.commits.len());
        summary.files = parse_numstat(&git(ws, &["diff", "--numstat", &mb, "HEAD"]).unwrap_or_default());
    }
    summary.uncommitted = count_porcelain(&git(ws, &["status", "--porcelain"]).unwrap_or_default());
    GitState::Ready(summary)
}

/// `git status --porcelain` entries in `ws`; 0 when it is not a git
/// checkout (a removed worktree has nothing left to lose).
pub fn uncommitted(ws: &Path) -> usize {
    count_porcelain(&git(ws, &["status", "--porcelain"]).unwrap_or_default())
}

/// `git log --format=%h%x09%s`.
pub fn parse_log(out: &str) -> Vec<Commit> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| match l.split_once('\t') {
            Some((sha, subject)) => Commit { sha: sha.trim().to_string(), subject: subject.trim().to_string() },
            None => Commit { sha: l.trim().to_string(), subject: String::new() },
        })
        .collect()
}

/// `git diff --numstat`: `added\tremoved\tpath`, `-` for binary files.
pub fn parse_numstat(out: &str) -> Vec<FileStat> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let (a, r, path) = (it.next()?, it.next()?, it.next()?);
            Some(FileStat { path: path.to_string(), added: a.parse().unwrap_or(0), removed: r.parse().unwrap_or(0) })
        })
        .collect()
}

/// Entries in `git status --porcelain`.
pub fn count_porcelain(out: &str) -> usize {
    out.lines().filter(|l| l.len() > 3).count()
}

// ── layout ──────────────────────────────────────────────────────────────────

/// Whether the session's pane area shows the report: no live process
/// behind it (a ptyd pane that exited, or a session that ended).
pub fn shows_report(st: &TuiState, r: &Row) -> bool {
    match st.backend_of(r.id()) {
        Backend::Ptyd { alive } => !alive,
        Backend::Legacy => r.session.status.is_terminal(),
    }
}

pub fn buttons(st: &TuiState, r: &Row) -> Vec<ReportButton> {
    let data = st.reports.get(r.id()).and_then(|s| s.data.as_ref());
    let mut v = vec![ReportButton::Remove];
    if data.is_some_and(|d| d.resumable) {
        v.push(ReportButton::Resume);
    }
    if data.and_then(|d| d.pr.as_ref()).and_then(|p| p.url.as_ref()).is_some() {
        v.push(ReportButton::OpenPr);
    }
    v
}

pub fn pr_url(st: &TuiState, id: &str) -> Option<String> {
    st.reports.get(id)?.data.as_ref()?.pr.as_ref()?.url.clone()
}

pub struct Plan {
    /// Pinned at the top: title, subtitle, gap, buttons, gap.
    pub head: Vec<Line<'static>>,
    /// Buttons on the `BUTTON_ROW` line: `(button, x offset, width)`.
    pub buttons: Vec<(ReportButton, u16, u16)>,
    /// Scrolls under the head.
    pub body: Vec<Line<'static>>,
    /// Body lines that open the PR when clicked.
    pub link_lines: Vec<usize>,
}

fn fmt_clock(ms: i64, now_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let (Some(t), Some(now)) = (Local.timestamp_millis_opt(ms).single(), Local.timestamp_millis_opt(now_ms).single()) else {
        return String::new();
    };
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%b %-d %H:%M").to_string()
    }
}

pub fn fmt_duration(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// Greedy word wrap to `width`, at most `max` lines (the last ends in `…`
/// when cut).
pub fn wrap(text: &str, width: usize, max: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut cut = false;
    'outer: for para in text.lines() {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            let need = if cur.is_empty() { word.width() } else { cur.width() + 1 + word.width() };
            if need > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                if out.len() == max {
                    cut = true;
                    break 'outer;
                }
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        if !cur.is_empty() {
            out.push(cur);
            if out.len() == max {
                cut = true;
                break;
            }
        }
    }
    if cut {
        if let Some(last) = out.last_mut() {
            *last = super::view::truncate(&format!("{last} …"), width);
        }
    }
    out.into_iter().map(|l| super::view::truncate(&l, width)).collect()
}

fn how_it_ended(st: &TuiState, r: &Row, data: Option<&ReportData>) -> String {
    let s = &r.session;
    let exit = st.panes.get(r.id()).filter(|p| !p.alive).map(|p| p.exit_code);
    match s.status {
        SessionStatus::Done if s.merged_at.is_some() || s.pr_number.is_some() => "finished · PR merged".into(),
        SessionStatus::Done => "finished".into(),
        SessionStatus::Interrupted => match data.and_then(|d| d.interrupt_cause.as_deref()) {
            Some(c) => format!("interrupted ({})", c.replace('_', " ")),
            None => "interrupted".into(),
        },
        SessionStatus::Terminated if s.terminal_at.is_some() => "the agent exited".into(),
        SessionStatus::Terminated => "stopped".into(),
        _ => match exit {
            Some(Some(code)) => format!("the agent exited (code {code})"),
            _ => "the agent exited".into(),
        },
    }
}

fn ended_ms(r: &Row, data: Option<&ReportData>) -> Option<i64> {
    let s = &r.session;
    s.terminal_at
        .or(data.and_then(|d| d.interrupted_at))
        .or(s.activity_since.filter(|_| s.status.is_terminal()))
        .filter(|&t| t >= s.started_at)
}

fn heading(p: &Palette, text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(p.faint).add_modifier(Modifier::BOLD)))
}

fn sep(p: &Palette) -> Span<'static> {
    Span::styled(" · ", Style::default().fg(p.faint))
}

/// The last non-empty lines of the final screen (live snapshot of an exited
/// ptyd pane, else the on-disk checkpoint).
pub fn last_screen(st: &TuiState, id: &str) -> Vec<String> {
    let Some(v) = st.views.get(id) else { return Vec::new() };
    let snap = match (&v.live, &v.checkpoint) {
        (Some(l), _) if super::pane::has_content(l) => l,
        (_, Some(cp)) => &cp.screen,
        _ => return Vec::new(),
    };
    let lines: Vec<String> = snap.lines[snap.scrollback_len.min(snap.lines.len())..]
        .iter()
        .map(|l| l.runs.iter().map(|r| r.text.as_str()).collect::<String>().trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(MAX_SCREEN_LINES)..].to_vec()
}

pub fn plan(st: &TuiState, r: &Row, width: u16) -> Plan {
    let p = &st.palette;
    let s = &r.session;
    let w = width as usize;
    let slot = st.reports.get(r.id());
    let data = slot.and_then(|sl| sl.data.as_ref());
    let faint = Style::default().fg(p.faint);
    let ink2 = Style::default().fg(p.ink_2);

    // Title: glyph, name, status badge.
    // The store may not have caught up with a pane that just exited.
    let exited = !s.status.is_terminal();
    let (glyph, gcolor) = super::view::status_glyph(st, r);
    let word = if exited { "exited" } else { super::view::status_word(&s.status) };
    let badge_color = if s.status == SessionStatus::Interrupted { p.review } else { p.ink_2 };
    let badge = vec![Span::styled(format!(" {word} "), Style::default().fg(badge_color).bg(p.card).add_modifier(Modifier::BOLD))];
    let name_w = w.saturating_sub(4 + super::view::spans_width(&badge));
    let title = Line::from(
        [
            vec![
                Span::styled(format!("{glyph} "), Style::default().fg(gcolor)),
                Span::styled(super::view::truncate(&s.name, name_w), Style::default().fg(p.ink).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
            ],
            badge,
        ]
        .concat(),
    );

    // Subtitle: how it ended · when · how long · cost · harness.
    let mut sub: Vec<Span<'static>> = vec![Span::raw("  "), Span::styled(how_it_ended(st, r, data), ink2)];
    if s.started_at > 0 {
        let start = fmt_clock(s.started_at, st.now_ms);
        let span = match ended_ms(r, data) {
            Some(end) => format!("{start} → {} ({})", fmt_clock(end, st.now_ms), fmt_duration(end - s.started_at)),
            None => format!("started {start}"),
        };
        sub.push(sep(p));
        sub.push(Span::styled(span, faint));
    }
    if s.cost_usd >= 0.005 {
        sub.push(sep(p));
        sub.push(Span::styled(format!("${:.2}", s.cost_usd), faint));
    }
    let harness = match &s.model {
        Some(m) => format!("{} · {m}", s.agent_type),
        None => s.agent_type.clone(),
    };
    if !harness.is_empty() {
        sub.push(sep(p));
        sub.push(Span::styled(harness, faint));
    }
    let mut kept: Vec<Span<'static>> = Vec::new();
    for span in sub {
        if super::view::spans_width(&kept) + span.content.width() > w {
            break;
        }
        kept.push(span);
    }
    if kept.last().is_some_and(|s| s.content == " · ") {
        kept.pop();
    }

    // Buttons.
    let mut buttons = Vec::new();
    let mut bline: Vec<Span<'static>> = vec![Span::raw("  ")];
    let mut x = 2u16;
    for b in super::report::buttons(st, r) {
        let text_w = b.text().width() as u16;
        if (x + text_w) as usize > w {
            break;
        }
        let bg = Style::default().bg(p.card);
        let label = if b == ReportButton::Remove { p.ci_failed } else { p.ink };
        bline.extend([
            Span::styled("[ ", bg.fg(p.faint)),
            Span::styled(b.key(), bg.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {} ", b.label()), bg.fg(label).add_modifier(Modifier::BOLD)),
            Span::styled("]", bg.fg(p.faint)),
            Span::raw("  "),
        ]);
        buttons.push((b, x, text_w));
        x += text_w + 2;
    }
    if slot.is_some_and(|sl| sl.loading) && (x as usize + 10) <= w {
        bline.push(Span::styled("loading…", faint));
    }
    let head = vec![title, Line::from(kept), Line::from(""), Line::from(bline), Line::from("")];

    // Body.
    let inner_w = w.saturating_sub(4);
    let mut body: Vec<Line<'static>> = Vec::new();
    let mut link_lines = Vec::new();
    let indent = |spans: Vec<Span<'static>>| Line::from([vec![Span::raw("  ")], spans].concat());

    let task = data.and_then(|d| d.task.clone()).or_else(|| s.summary.clone());
    if task.is_some() || s.activity_note.is_some() {
        body.push(heading(p, "TASK"));
        if let Some(t) = task {
            for l in wrap(&t, inner_w, MAX_TASK_LINES) {
                body.push(indent(vec![Span::styled(l, Style::default().fg(p.ink))]));
            }
        }
        if let Some(n) = &s.activity_note {
            body.push(indent(vec![Span::styled(super::view::truncate(&format!("note: {n}"), inner_w), faint)]));
        }
        body.push(Line::from(""));
    }

    if let Some(number) = s.pr_number {
        body.push(heading(p, "PULL REQUEST"));
        let pr = data.and_then(|d| d.pr.as_ref());
        let link = Style::default().fg(p.ink).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let mut first = vec![Span::styled(format!("#{number}"), link)];
        if let Some(t) = pr.and_then(|p| p.title.as_ref()) {
            first.push(Span::raw(" "));
            first.push(Span::styled(super::view::truncate(t, inner_w.saturating_sub(8)), Style::default().fg(p.ink)));
        }
        link_lines.push(body.len());
        body.push(indent(first));
        let mut facts: Vec<Span<'static>> = Vec::new();
        let state = if s.merged_at.is_some() || s.status == SessionStatus::Done {
            Some(("merged", p.done))
        } else {
            match s.status {
                SessionStatus::Mergeable => Some(("mergeable", p.mergeable)),
                SessionStatus::ReviewPending => Some(("in review", p.review)),
                SessionStatus::CiFailed => Some(("CI failed", p.ci_failed)),
                SessionStatus::PrOpen => Some(("open", p.pr_open)),
                _ => None,
            }
        };
        if let Some((word, c)) = state {
            facts.push(Span::styled(word, Style::default().fg(c)));
        }
        let ci = match pr.and_then(|p| p.ci.as_ref()).filter(|c| c.total > 0) {
            Some(c) if c.failing > 0 => Some((format!("CI ✗ {} failing", c.failing), p.ci_failed)),
            Some(c) if c.pending > 0 => Some((format!("CI ◌ {}/{} passing", c.passing, c.total), p.faint)),
            Some(c) => Some((format!("CI ✓ {}/{}", c.passing, c.total), p.working)),
            None => s.gate_status.as_ref().and_then(|g| super::view::ci_glyph(&g.ci, p)).map(|(g, c)| (format!("CI {g}"), c)),
        };
        if let Some((t, c)) = ci {
            if !facts.is_empty() {
                facts.push(sep(p));
            }
            facts.push(Span::styled(t, Style::default().fg(c)));
        }
        let review = s.gate_status.as_ref().and_then(|g| match g.review {
            GateCheck::Passing => Some(("approved", p.working)),
            GateCheck::Failing => Some(("changes requested", p.ci_failed)),
            GateCheck::Pending => Some(("awaiting review", p.faint)),
            GateCheck::Unknown => None,
        });
        if let Some((t, c)) = review {
            if !facts.is_empty() {
                facts.push(sep(p));
            }
            facts.push(Span::styled(t, Style::default().fg(c)));
        }
        if !facts.is_empty() {
            body.push(indent(facts));
        }
        if let Some(url) = pr.and_then(|p| p.url.as_ref()) {
            link_lines.push(body.len());
            body.push(indent(vec![Span::styled(super::view::truncate(url, inner_w), Style::default().fg(p.faint).add_modifier(Modifier::UNDERLINED))]));
        }
        body.push(Line::from(""));
    }

    body.push(heading(p, "WHAT IT DID"));
    match data.map(|d| &d.git) {
        None => body.push(indent(vec![Span::styled("loading…", faint)])),
        Some(GitState::NoWorkspace) => body.push(indent(vec![Span::styled("no workspace recorded", faint)])),
        Some(GitState::Missing(path)) => {
            body.push(indent(vec![Span::styled(super::view::truncate(&format!("workspace is gone: {path}"), inner_w), faint)]))
        }
        Some(GitState::NotRepo) => body.push(indent(vec![Span::styled("the workspace is not a git repository", faint)])),
        Some(GitState::Ready(g)) => {
            let mut top = Vec::new();
            if let Some(b) = &g.branch {
                top.push(Span::styled(format!("⎇ {b}"), Style::default().fg(p.ink_2)));
            }
            if let Some(base) = &g.base {
                if !top.is_empty() {
                    top.push(sep(p));
                }
                let n = g.commits_total;
                let s = if n == 1 { "" } else { "s" };
                top.push(Span::styled(format!("{n} commit{s} ahead of {base}"), faint));
            }
            if !top.is_empty() {
                body.push(indent(top));
            }
            for c in &g.commits {
                body.push(indent(vec![
                    Span::styled(format!("{} ", c.sha), Style::default().fg(p.accent)),
                    Span::styled(super::view::truncate(&c.subject, inner_w.saturating_sub(c.sha.width() + 1)), Style::default().fg(p.ink)),
                ]));
            }
            if g.commits_total > g.commits.len() {
                body.push(indent(vec![Span::styled(format!("+ {} more", g.commits_total - g.commits.len()), faint)]));
            }
            if !g.files.is_empty() {
                let n = g.files.len();
                let s = if n == 1 { "" } else { "s" };
                body.push(indent(vec![
                    Span::styled(format!("{n} file{s} changed  "), Style::default().fg(p.ink_2)),
                    Span::styled(format!("+{}", g.insertions()), Style::default().fg(p.working)),
                    Span::raw(" "),
                    Span::styled(format!("−{}", g.deletions()), Style::default().fg(p.ci_failed)),
                ]));
                for f in g.files.iter().take(MAX_FILES) {
                    let stat = format!("  +{} −{}", f.added, f.removed);
                    body.push(indent(vec![
                        Span::styled(format!("  {}", super::view::truncate(&f.path, inner_w.saturating_sub(stat.width() + 2))), Style::default().fg(p.ink_2)),
                        Span::styled(stat, faint),
                    ]));
                }
                if n > MAX_FILES {
                    body.push(indent(vec![Span::styled(format!("  + {} more files", n - MAX_FILES), faint)]));
                }
            } else if g.base.is_some() && g.commits_total == 0 {
                body.push(indent(vec![Span::styled("no commits of its own", faint)]));
            }
            if g.uncommitted > 0 {
                let n = g.uncommitted;
                let s = if n == 1 { "" } else { "s" };
                body.push(indent(vec![Span::styled(format!("● {n} uncommitted change{s}"), Style::default().fg(p.review))]));
            }
        }
    }
    body.push(Line::from(""));

    let screen = last_screen(st, r.id());
    if !screen.is_empty() {
        body.push(heading(p, "LAST SCREEN"));
        for l in screen {
            body.push(indent(vec![Span::styled(super::view::truncate(&l, inner_w), faint)]));
        }
        body.push(Line::from(""));
    }

    if let Some(ws) = &s.workspace_path {
        body.push(Line::from(vec![
            Span::styled("WORKSPACE  ", Style::default().fg(p.faint).add_modifier(Modifier::BOLD)),
            Span::styled(super::view::truncate(ws, w.saturating_sub(11)), faint),
        ]));
    }
    body.push(Line::from(vec![
        Span::styled("ID         ", Style::default().fg(p.faint).add_modifier(Modifier::BOLD)),
        Span::styled(super::view::truncate(&s.id, w.saturating_sub(11)), faint),
    ]));

    Plan { head, buttons, body, link_lines }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_log_numstat_and_porcelain() {
        let log = "a1b2c3d\tfix: refresh the token before expiry\nbeef123\tchore: tidy\n\n";
        assert_eq!(
            parse_log(log),
            vec![
                Commit { sha: "a1b2c3d".into(), subject: "fix: refresh the token before expiry".into() },
                Commit { sha: "beef123".into(), subject: "chore: tidy".into() },
            ]
        );
        let numstat = "10\t2\tsrc/auth.rs\n-\t-\tassets/logo.png\n3\t0\tpath with\ttab.md\n";
        let files = parse_numstat(numstat);
        assert_eq!(files.len(), 3);
        assert_eq!(files[1], FileStat { path: "assets/logo.png".into(), added: 0, removed: 0 });
        assert_eq!(files[2].path, "path with\ttab.md");
        let g = GitSummary { files, ..Default::default() };
        assert_eq!((g.insertions(), g.deletions()), (13, 2));
        assert_eq!(count_porcelain(" M src/a.rs\n?? new.txt\n\n"), 2);
        assert_eq!(count_porcelain(""), 0);
    }

    #[test]
    fn collect_git_reports_commits_ahead_of_the_base_branch() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(ws)
                .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["commit", "-q", "--allow-empty", "-m", "init"]);
        run(&["checkout", "-q", "-b", "feat"]);
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "feat: add a"]);
        std::fs::write(ws.join("b.txt"), "dirty").unwrap();
        let GitState::Ready(g) = collect_git(ws) else { panic!("not ready") };
        assert_eq!(g.branch.as_deref(), Some("feat"));
        assert_eq!(g.base.as_deref(), Some("main"));
        assert_eq!(g.commits_total, 1);
        assert_eq!(g.commits[0].subject, "feat: add a");
        assert_eq!((g.files.len(), g.insertions()), (1, 2));
        assert_eq!(g.uncommitted, 1);
        assert_eq!(collect_git(&ws.join("nope")), GitState::Missing(ws.join("nope").display().to_string()));
    }

    #[test]
    fn wrap_cuts_long_text_to_its_line_budget() {
        let lines = wrap("one two three four five six seven eight nine ten", 12, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with('…'), "{lines:?}");
        assert!(lines.iter().all(|l| l.width() <= 12));
        assert_eq!(wrap("short", 40, 4), vec!["short"]);
    }

    #[test]
    fn pr_urls_fall_back_to_the_repo_slug() {
        assert_eq!(github_pr_url("acme/web", 7).as_deref(), Some("https://github.com/acme/web/pull/7"));
        assert_eq!(github_pr_url("/abs/path", 7), None);
        assert_eq!(github_pr_url("web", 7), None);
    }

    #[test]
    fn durations_are_compact() {
        assert_eq!(fmt_duration(45_000), "45s");
        assert_eq!(fmt_duration(39 * 60_000), "39m");
        assert_eq!(fmt_duration(2 * 3_600_000 + 5 * 60_000), "2h 05m");
    }
}
