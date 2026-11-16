-- Schema of a store written by ninox v0.37.0 (tag v0.37.0): its Store::open DDL
-- and column migrations, applied to an empty database and dumped with .schema.
CREATE TABLE sessions (
                id TEXT PRIMARY KEY, orchestrator_id TEXT,
                name TEXT NOT NULL, repo TEXT NOT NULL,
                status TEXT NOT NULL, agent_type TEXT NOT NULL,
                cost_usd REAL NOT NULL DEFAULT 0, started_at INTEGER NOT NULL,
                pr_number INTEGER, pr_id INTEGER,
                workspace_path TEXT, pid INTEGER,
                model TEXT, context_tokens INTEGER
            , catalogue_path TEXT, context_used_pct REAL, context_total_tokens INTEGER, context_window_size INTEGER, claude_session_id TEXT, summary TEXT, terminal_at INTEGER, gate_status TEXT, merged_at INTEGER, activity TEXT, activity_note TEXT, activity_since INTEGER, machine_id TEXT);
CREATE TABLE orchestrators (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL
            );
CREATE TABLE prs (
                id INTEGER PRIMARY KEY, number INTEGER NOT NULL,
                title TEXT NOT NULL, url TEXT NOT NULL,
                body TEXT NOT NULL, session_id TEXT NOT NULL
            );
CREATE TABLE ci_status (
                pr_id INTEGER PRIMARY KEY, total INTEGER NOT NULL,
                passing INTEGER NOT NULL, failing INTEGER NOT NULL,
                pending INTEGER NOT NULL
            );
CREATE TABLE review_comments (
                id INTEGER PRIMARY KEY, pr_id INTEGER NOT NULL,
                author TEXT NOT NULL, body TEXT NOT NULL,
                path TEXT, line INTEGER, created_at INTEGER NOT NULL
            );
CREATE TABLE pr_watches (
                repo TEXT NOT NULL, pr_number INTEGER NOT NULL,
                pr_url TEXT NOT NULL,
                opener_session_id TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                PRIMARY KEY (repo, pr_number, opener_session_id)
            );
CREATE TABLE orchestrator_runtimes (
                orchestrator_id TEXT PRIMARY KEY,
                runtime_id TEXT NOT NULL UNIQUE,
                server_epoch TEXT NOT NULL,
                physical_tmux_name TEXT NOT NULL,
                pane_id TEXT NOT NULL,
                root_pid INTEGER NOT NULL,
                root_created_at INTEGER NOT NULL,
                registered_at INTEGER NOT NULL
            );
CREATE TABLE orchestrator_plans (
                orchestrator_id TEXT PRIMARY KEY,
                file_path TEXT NOT NULL,
                registered_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
CREATE TABLE session_deps (
                session_id TEXT NOT NULL,
                depends_on TEXT NOT NULL,
                kind TEXT NOT NULL,
                note TEXT,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, depends_on, kind)
            );
CREATE TABLE session_messages (
                session_id TEXT PRIMARY KEY,
                delivered_count INTEGER NOT NULL
            );
CREATE TABLE fleet_sessions (
        session_id      TEXT PRIMARY KEY,
        task_brief      TEXT,
        branch          TEXT,
        interrupted_at  INTEGER,
        last_status     TEXT,
        interrupt_cause TEXT,
        restored_at     INTEGER,
        restore_mode    TEXT,
        restore_note    TEXT,
        reconciled_terminal_at INTEGER
    );
CREATE TABLE work_requests (
        id              TEXT PRIMARY KEY,
        from_session    TEXT NOT NULL,
        orchestrator_id TEXT,
        body            TEXT NOT NULL,
        created_at      INTEGER NOT NULL,
        delivered_at    INTEGER,
        resolved_at     INTEGER
    );
CREATE TABLE fleet_recoveries (
        orchestrator_id  TEXT PRIMARY KEY,
        interrupted_at   INTEGER,
        restored_at      INTEGER NOT NULL,
        briefing_sent_at INTEGER,
        acked_at         INTEGER
    );
CREATE TABLE fleet_state (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
CREATE TABLE fleet_lock (
        name        TEXT PRIMARY KEY,
        holder      TEXT NOT NULL,
        acquired_at INTEGER NOT NULL
    );
