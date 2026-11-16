use super::*;
use crate::config::{LimitsConfig, MachineProfile};
use crate::fleet::snapshot::test_support::session;
use crate::store::{Admission, AdmitError, HeldMessage, NewQueueEntry, QueueKind, Store};
use crate::test_isolation::isolate;
use crate::types::{MessageKind, SessionStatus};

fn store_in(iso: &crate::test_isolation::Isolation) -> Arc<Store> {
    Arc::new(Store::open(iso.root().join("ninox.db")).unwrap())
}

fn running(store: &Store, id: &str, machine: Option<&str>) {
    let mut s = session(id, None, SessionStatus::Working, 1);
    s.machine_id = machine.map(str::to_string);
    store.upsert_session(&s).unwrap();
}

fn spawn_req(id: &str) -> Session {
    session(id, None, SessionStatus::Working, 1)
}

fn admit(store: &Store, id: &str, limit: Option<u32>, no_queue: bool) -> Result<Admission, AdmitError> {
    let s = spawn_req(id);
    store.admit_session(NewQueueEntry { kind: QueueKind::Spawn, session: &s, payload: "{}" }, limit, no_queue, 10)
}

#[test]
fn counting_excludes_the_prompt_agent_and_terminal_states() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "w1", None);
    for (id, status) in [
        ("done", SessionStatus::Done),
        ("term", SessionStatus::Terminated),
        ("intr", SessionStatus::Interrupted),
    ] {
        store.upsert_session(&session(id, None, status, 1)).unwrap();
    }
    let mut prompt = session(crate::worker_type::PROMPT_SESSION_ID, None, SessionStatus::Working, 1);
    prompt.worker_type = Some(crate::worker_type::PROMPT_WORKER_TYPE.into());
    store.upsert_session(&prompt).unwrap();
    for (id, status) in [("pr", SessionStatus::PrOpen), ("ci", SessionStatus::CiFailed), ("rev", SessionStatus::AwaitingReview)] {
        store.upsert_session(&session(id, None, status, 1)).unwrap();
    }
    assert_eq!(store.running_session_count(None).unwrap(), 4);
}

#[test]
fn the_limit_is_per_location() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "l1", None);
    running(&store, "r1", Some("gpu"));
    running(&store, "r2", Some("gpu"));
    assert_eq!(store.running_session_count(None).unwrap(), 1);
    assert_eq!(store.running_session_count(Some("gpu")).unwrap(), 2);

    let mut remote = spawn_req("r3");
    remote.machine_id = Some("gpu".into());
    let queued = store
        .admit_session(NewQueueEntry { kind: QueueKind::Spawn, session: &remote, payload: "{}" }, Some(2), false, 1)
        .unwrap();
    assert_eq!(queued, Admission::Queued { ahead: 0 });
    assert_eq!(admit(&store, "l2", Some(2), false).unwrap(), Admission::Admitted);
}

#[test]
fn over_the_limit_queues_and_no_queue_fails_fast() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    running(&store, "b", None);
    assert_eq!(admit(&store, "c", Some(2), false).unwrap(), Admission::Queued { ahead: 0 });
    assert_eq!(admit(&store, "d", Some(2), false).unwrap(), Admission::Queued { ahead: 1 });
    match admit(&store, "e", Some(2), true) {
        Err(AdmitError::LimitReached { running: 2, limit: 2 }) => {}
        other => panic!("expected LimitReached, got {other:?}"),
    }
    assert!(store.queue_entry("e").unwrap().is_none(), "--no-queue must leave nothing behind");
    assert!(matches!(admit(&store, "c", Some(2), false), Err(AdmitError::AlreadyQueued)));
    assert!(matches!(admit(&store, "a", Some(2), false), Err(AdmitError::IdTaken)));
}

#[test]
fn unlimited_admits_everything() {
    let iso = isolate();
    let store = store_in(&iso);
    for i in 0..20 {
        running(&store, &format!("w{i}"), None);
    }
    assert_eq!(admit(&store, "x", None, false).unwrap(), Admission::Admitted);
}

#[test]
fn queued_sessions_start_first_in_first_out_when_a_slot_frees() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    running(&store, "b", None);
    for id in ["q1", "q2", "q3"] {
        admit(&store, id, Some(2), false).unwrap();
    }
    assert!(store.claim_queued(None, Some(2), 20).unwrap().is_empty());

    let mut a = store.get_session("a").unwrap().unwrap();
    a.status = SessionStatus::Done;
    store.upsert_session(&a).unwrap();
    let claimed: Vec<String> = store.claim_queued(None, Some(2), 20).unwrap().into_iter().map(|e| e.id).collect();
    assert_eq!(claimed, ["q1"]);
    // The claimed entry holds the slot until it's released.
    assert!(store.claim_queued(None, Some(2), 20).unwrap().is_empty());
    assert_eq!(store.running_session_count(None).unwrap(), 2);

    // Its row is written; the reservation must not count it twice.
    store.upsert_session(&spawn_req("q1")).unwrap();
    assert_eq!(store.running_session_count(None).unwrap(), 2);
    store.release_queue_entry("q1").unwrap();
    assert_eq!(store.running_session_count(None).unwrap(), 2);

    let mut b = store.get_session("b").unwrap().unwrap();
    b.status = SessionStatus::Terminated;
    store.upsert_session(&b).unwrap();
    let claimed: Vec<String> = store.claim_queued(None, Some(2), 30).unwrap().into_iter().map(|e| e.id).collect();
    assert_eq!(claimed, ["q2"]);
}

#[test]
fn a_new_spawn_waits_behind_the_queue_even_with_a_free_slot() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    admit(&store, "q1", Some(1), false).unwrap();
    let mut a = store.get_session("a").unwrap().unwrap();
    a.status = SessionStatus::Done;
    store.upsert_session(&a).unwrap();
    assert_eq!(admit(&store, "late", Some(1), false).unwrap(), Admission::Queued { ahead: 1 });
    let claimed: Vec<String> = store.claim_queued(None, Some(1), 20).unwrap().into_iter().map(|e| e.id).collect();
    assert_eq!(claimed, ["q1"]);
}

#[test]
fn raising_the_limit_frees_slots_on_the_next_claim() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    for id in ["q1", "q2", "q3"] {
        admit(&store, id, Some(1), false).unwrap();
    }
    let claimed: Vec<String> = store.claim_queued(None, Some(3), 20).unwrap().into_iter().map(|e| e.id).collect();
    assert_eq!(claimed, ["q1", "q2"]);
}

#[test]
fn lowering_the_limit_kills_nothing() {
    let iso = isolate();
    let store = store_in(&iso);
    for id in ["a", "b", "c", "d"] {
        running(&store, id, None);
    }
    assert_eq!(admit(&store, "q", Some(2), false).unwrap(), Admission::Queued { ahead: 0 });
    assert!(store.claim_queued(None, Some(2), 20).unwrap().is_empty());
    for id in ["a", "b", "c", "d"] {
        assert_eq!(store.get_session(id).unwrap().unwrap().status, SessionStatus::Working);
    }
}

#[test]
fn a_dead_launchers_reservation_is_released() {
    let iso = isolate();
    let store = store_in(&iso);
    admit(&store, "q", Some(1), false).unwrap();
    {
        let conn = rusqlite::Connection::open(iso.root().join("ninox.db")).unwrap();
        // Arbitrary pid far above any real one on macOS/Linux.
        conn.execute("UPDATE session_queue SET claimed_by_pid = 999999999 WHERE id = 'q'", []).unwrap();
    }
    assert_eq!(store.running_session_count(None).unwrap(), 1);
    assert_eq!(admit(&store, "next", Some(1), false).unwrap(), Admission::Admitted);
    assert!(store.queue_entry("q").unwrap().is_none());
}

/// Separate connections, as separate processes would have: the count and
/// the reservation must be one atomic step, or both racers see a free slot.
#[test]
fn simultaneous_spawns_cannot_both_take_the_last_slot() {
    let iso = isolate();
    for round in 0..10 {
        let db = iso.root().join(format!("spawn{round}.db"));
        Store::open(&db).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let db = db.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = Store::open(&db).unwrap();
                    barrier.wait();
                    let s = spawn_req(&format!("r{round}-{i}"));
                    store
                        .admit_session(NewQueueEntry { kind: QueueKind::Spawn, session: &s, payload: "{}" }, Some(1), false, 1)
                        .unwrap()
                })
            })
            .collect();
        let admitted = handles.into_iter().map(|h| h.join().unwrap()).filter(|a| *a == Admission::Admitted).count();
        assert_eq!(admitted, 1, "round {round}: exactly one spawn may take the one free slot");
    }
}

/// The interleaving atomicity rules out: the spawn counts a free slot, the
/// drain claims it, then the spawn sees nobody queued ahead and admits too.
#[test]
fn a_drain_and_a_spawn_cannot_both_take_the_last_slot() {
    let iso = isolate();
    for round in 0..20 {
        let db = iso.root().join(format!("race{round}.db"));
        let store = Store::open(&db).unwrap();
        running(&store, "blocker", None);
        admit(&store, "q", Some(1), false).unwrap();
        let mut b = store.get_session("blocker").unwrap().unwrap();
        b.status = SessionStatus::Done;
        store.upsert_session(&b).unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let (db1, b1) = (db.clone(), barrier.clone());
        let drain = std::thread::spawn(move || {
            let st = Store::open(&db1).unwrap();
            b1.wait();
            st.claim_queued(None, Some(1), 2).unwrap().len()
        });
        let (db2, b2) = (db.clone(), barrier.clone());
        let spawn = std::thread::spawn(move || {
            let st = Store::open(&db2).unwrap();
            b2.wait();
            admit(&st, "s", Some(1), false).unwrap()
        });
        let drained = drain.join().unwrap();
        let spawned = spawn.join().unwrap();
        assert_eq!(drained, 1, "round {round}");
        // Either order is a valid serialisation (ahead 1 if the spawn ran
        // first, 0 if the drain did); being admitted never is.
        assert!(matches!(spawned, Admission::Queued { .. }), "round {round}: the spawn jumped the line: {spawned:?}");
        assert_eq!(store.running_session_count(None).unwrap(), 1, "round {round}");
    }
}

#[test]
fn the_sessions_table_never_holds_a_queued_status() {
    let iso = isolate();
    let store = store_in(&iso);
    let s = session("q", None, SessionStatus::Queued, 1);
    assert!(store.upsert_session(&s).is_err());
    assert!(store.get_session("q").unwrap().is_none());
}

#[test]
fn cancelling_a_queued_session_drops_it_and_its_held_messages() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    admit(&store, "q", Some(1), false).unwrap();
    let msg = HeldMessage { tagged: "[freeform] hi".into(), attributed_from: None, defanged: "hi".into(), kind: MessageKind::Freeform };
    assert!(store.hold_message_if_queued("q", &msg, 1).unwrap());
    assert!(store.cancel_queued("q").unwrap());
    assert!(store.queue_entry("q").unwrap().is_none());
    assert_eq!(store.held_message_count("q").unwrap(), 0);
    assert!(!store.hold_message_if_queued("q", &msg, 1).unwrap(), "nothing left to hold for");
}

#[tokio::test(flavor = "current_thread")]
async fn messages_to_a_queued_session_are_held_then_delivered_on_start() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    admit(&store, "q", Some(1), false).unwrap();
    let dir = iso.root().join("sessions");
    for text in ["first", "second"] {
        crate::messaging::deliver_message(
            &store, &dir, "q", text, crate::config::SendMechanism::Keystrokes,
            None, None, MessageKind::Freeform,
        )
        .await
        .expect("a send to a queued session succeeds without a pane");
    }
    assert_eq!(store.held_message_count("q").unwrap(), 2);

    // Claimed from the queue, it keeps holding until its launcher flushes,
    // so a send during startup can't overtake the earlier ones.
    let mut a = store.get_session("a").unwrap().unwrap();
    a.status = SessionStatus::Done;
    store.upsert_session(&a).unwrap();
    assert_eq!(store.claim_queued(None, Some(1), 5).unwrap().len(), 1);
    let msg = HeldMessage { tagged: "x".into(), attributed_from: None, defanged: "third".into(), kind: MessageKind::Freeform };
    assert!(store.hold_message_if_queued("q", &msg, 1).unwrap());

    let held = store.take_held_messages("q").unwrap();
    let texts: Vec<&str> = held.iter().map(|m| m.defanged.as_str()).collect();
    assert_eq!(texts, ["first", "second", "third"], "delivered in send order");
    assert!(held[0].tagged.starts_with("[freeform]"));

    // Released, it delivers directly again.
    store.release_queue_entry("q").unwrap();
    assert!(!store.hold_message_if_queued("q", &msg, 1).unwrap());
}

#[test]
fn an_admitted_spawn_keeps_direct_delivery_during_its_launch() {
    let iso = isolate();
    let store = store_in(&iso);
    assert_eq!(admit(&store, "direct", Some(1), false).unwrap(), Admission::Admitted);
    let msg = HeldMessage { tagged: "x".into(), attributed_from: None, defanged: "x".into(), kind: MessageKind::Freeform };
    assert!(!store.hold_message_if_queued("direct", &msg, 1).unwrap());
}

#[test]
fn stale_held_messages_never_reach_a_later_session_with_the_same_id() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    admit(&store, "q", Some(1), false).unwrap();
    let msg = HeldMessage { tagged: "x".into(), attributed_from: None, defanged: "old".into(), kind: MessageKind::Freeform };
    assert!(store.hold_message_if_queued("q", &msg, 1).unwrap());
    store.drop_queue_entry("q").unwrap();
    assert_eq!(store.held_message_count("q").unwrap(), 0);

    // A leftover from a crashed launcher's era is cleared when the id is
    // queued again.
    {
        let conn = rusqlite::Connection::open(iso.root().join("ninox.db")).unwrap();
        conn.execute(
            "INSERT INTO held_messages (to_id, tagged, defanged, kind, created_at) VALUES ('q', 'x', 'old', 'freeform', 1)",
            [],
        ).unwrap();
    }
    admit(&store, "q", Some(1), false).unwrap();
    assert_eq!(store.held_message_count("q").unwrap(), 0);
}

#[test]
fn positions_and_overlay_reflect_the_queue() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    store.upsert_session(&session("intr", None, SessionStatus::Interrupted, 1)).unwrap();
    admit(&store, "q1", Some(1), false).unwrap();
    let intr = store.get_session("intr").unwrap().unwrap();
    store
        .admit_session(NewQueueEntry { kind: QueueKind::Relaunch, session: &intr, payload: "{}" }, Some(1), false, 11)
        .unwrap();
    let mut remote = spawn_req("rq");
    remote.machine_id = Some("gpu".into());
    store.admit_session(NewQueueEntry { kind: QueueKind::Spawn, session: &remote, payload: "{}" }, None, false, 12).unwrap();

    let queue = store.list_queue().unwrap();
    let pos = queue_positions(&queue);
    assert_eq!(pos.get("q1"), Some(&1));
    assert_eq!(pos.get("intr"), Some(&2));
    assert_eq!(pos.get("rq"), None, "admitted (launching) entries have no position");

    let mut sessions = store.list_sessions().unwrap();
    overlay_queue(&mut sessions, &queue);
    let status = |id: &str| sessions.iter().find(|s| s.id == id).map(|s| s.status.clone());
    assert_eq!(status("q1"), Some(SessionStatus::Queued));
    assert_eq!(status("intr"), Some(SessionStatus::Queued));
    assert_eq!(status("rq"), None, "a launching spawn shows only once its real row exists");
    assert_eq!(status("a"), Some(SessionStatus::Working));
}

#[test]
fn summary_headline_matches_the_brief() {
    let iso = isolate();
    let store = store_in(&iso);
    let mut config = AppConfig {
        limits: LimitsConfig { max_concurrent_local: 2, ..Default::default() },
        ..Default::default()
    };
    config.remote_machines.machines.push(MachineProfile {
        id: "m1".into(), label: "gpu-box".into(), ssh_target: "gpu".into(),
        remote_session: "default".into(), enabled: true,
    });
    running(&store, "a", None);
    running(&store, "b", None);
    admit(&store, "q1", Some(2), false).unwrap();
    admit(&store, "q2", Some(2), false).unwrap();
    let summary = summarize(&store, &config).unwrap();
    assert_eq!(summary[0].headline(), "2/2 workers running locally · 2 queued");
    assert_eq!(summary[1].headline(), "0 workers running on gpu-box");
}

#[test]
fn locations_parse_by_name_id_or_label() {
    let mut config = AppConfig::default();
    config.remote_machines.machines.push(MachineProfile {
        id: "m1".into(), label: "gpu-box".into(), ssh_target: "gpu".into(),
        remote_session: "default".into(), enabled: true,
    });
    assert_eq!(parse_location(&config, "local").unwrap(), None);
    assert_eq!(parse_location(&config, "m1").unwrap(), Some("m1".into()));
    assert_eq!(parse_location(&config, "gpu-box").unwrap(), Some("m1".into()));
    assert!(parse_location(&config, "nope").is_err());
}

#[test]
fn limits_round_trip_through_the_guarded_config_update() {
    let _iso = isolate();
    let (saved, ()) = AppConfig::update(|c| {
        c.limits.set_limit(None, Some(3));
        c.limits.set_limit(Some("m1"), Some(5));
    })
    .unwrap();
    assert_eq!(saved.limits.limit_for(None), Some(3));
    let reloaded = AppConfig::load().unwrap();
    assert_eq!(reloaded.limits, saved.limits);
    assert_eq!(reloaded.limits.limit_for(Some("m1")), Some(5));
    assert_eq!(reloaded.limits.limit_for(Some("other")), None, "absent remote = unlimited");

    AppConfig::update(|c| {
        c.limits.set_limit(None, None);
        c.limits.set_limit(Some("m1"), None);
    })
    .unwrap();
    let reloaded = AppConfig::load().unwrap();
    assert_eq!(reloaded.limits.limit_for(None), None, "0 = unlimited");
    assert!(reloaded.limits.remote.is_empty());

    // The guard: a hand-broken file is refused, not overwritten.
    std::fs::write(AppConfig::config_path(), "limits = [").unwrap();
    assert!(AppConfig::update(|c| c.limits.set_limit(None, Some(4))).is_err());
}

#[test]
fn the_default_local_limit_is_six() {
    assert_eq!(AppConfig::default().limits.limit_for(None), Some(6));
    let cfg: AppConfig = toml::from_str("port = 8080\n").unwrap();
    assert_eq!(cfg.limits.limit_for(None), Some(6));
}

#[tokio::test(flavor = "current_thread")]
async fn removing_a_queued_session_just_drops_it_from_the_queue() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    admit(&store, "q", Some(1), false).unwrap();
    let engine = crate::events::Engine::new(store.clone());
    engine.remove_session("q", false).await.unwrap();
    assert!(store.queue_entry("q").unwrap().is_none());
    assert_eq!(store.get_session("a").unwrap().unwrap().status, SessionStatus::Working, "running sessions untouched");
}

#[tokio::test(flavor = "current_thread")]
async fn removing_an_orchestrator_drops_its_queued_workers() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    store.upsert_session(&session("boss", None, SessionStatus::Working, 1)).unwrap();
    store.upsert_orchestrator(&crate::types::Orchestrator { id: "boss".into(), name: "boss".into(), created_at: 0 }).unwrap();
    let mine = session("mine", Some("boss"), SessionStatus::Working, 1);
    let other = session("other", Some("someone-else"), SessionStatus::Working, 1);
    for s in [&mine, &other] {
        store.admit_session(NewQueueEntry { kind: QueueKind::Spawn, session: s, payload: "{}" }, Some(1), false, 1).unwrap();
    }
    let engine = crate::events::Engine::new(store.clone());
    engine.remove_orchestrator_keeping_live("boss").await.unwrap();
    assert!(store.queue_entry("mine").unwrap().is_none());
    assert!(store.queue_entry("other").unwrap().is_some(), "another orchestrator's queue is left alone");
}

#[test]
fn a_manual_resume_takes_a_queued_restore_out_of_line_but_keeps_its_messages() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "a", None);
    let intr = session("intr", None, SessionStatus::Interrupted, 1);
    store.upsert_session(&intr).unwrap();
    store.admit_session(NewQueueEntry { kind: QueueKind::Relaunch, session: &intr, payload: "{}" }, Some(1), false, 1).unwrap();
    let msg = HeldMessage { tagged: "x".into(), attributed_from: None, defanged: "keep".into(), kind: MessageKind::Freeform };
    assert!(store.hold_message_if_queued("intr", &msg, 1).unwrap());
    assert!(store.dequeue_waiting("intr").unwrap());
    assert!(store.queue_entry("intr").unwrap().is_none());
    assert_eq!(store.held_message_count("intr").unwrap(), 1);
}

fn orch(store: &Store, id: &str) {
    store.upsert_orchestrator(&crate::types::Orchestrator { id: id.into(), name: id.into(), created_at: 0 }).unwrap();
    running(store, id, None);
}

fn admit_orchestrator(store: &Store, id: &str, limit: Option<u32>, no_queue: bool) -> Result<Admission, AdmitError> {
    let s = spawn_req(id);
    store.admit_session(
        NewQueueEntry { kind: QueueKind::Spawn, session: &s, payload: r#"{"kind":"orchestrator"}"# },
        limit, no_queue, 10,
    )
}

#[test]
fn orchestrators_do_not_count_toward_the_limit() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "w1", None);
    orch(&store, "o1");
    orch(&store, "o2");
    assert_eq!(store.running_session_count(None).unwrap(), 1);
    assert_eq!(store.running_orchestrator_count(None).unwrap(), 2);
    assert!(matches!(admit(&store, "w2", Some(2), false).unwrap(), Admission::Admitted));
}

#[test]
fn orchestrator_spawn_is_admitted_at_the_limit_and_not_counted() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "w1", None);
    assert!(matches!(admit(&store, "w2", Some(1), false).unwrap(), Admission::Queued { .. }));
    assert!(matches!(admit_orchestrator(&store, "o1", Some(1), true).unwrap(), Admission::Admitted));
    assert_eq!(store.running_session_count(None).unwrap(), 1, "launching orchestrator reserves no worker slot");
    assert!(matches!(admit(&store, "w3", Some(1), true), Err(AdmitError::LimitReached { .. })));
}

#[test]
fn orchestrator_resume_is_admitted_at_the_limit() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "w1", None);
    store.upsert_orchestrator(&crate::types::Orchestrator { id: "o1".into(), name: "o1".into(), created_at: 0 }).unwrap();
    let dead = session("o1", None, SessionStatus::Interrupted, 1);
    store.upsert_session(&dead).unwrap();
    let a = store
        .admit_session(NewQueueEntry { kind: QueueKind::Relaunch, session: &dead, payload: "{}" }, Some(1), true, 10)
        .unwrap();
    assert!(matches!(a, Admission::Admitted));
}

#[test]
fn a_worker_with_a_forged_orchestrator_worker_type_still_counts_and_queues() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "w1", None);
    let mut forged = spawn_req("sneaky");
    forged.worker_type = Some("Orchestrator".into());
    let a = store
        .admit_session(
            NewQueueEntry { kind: QueueKind::Spawn, session: &forged, payload: r#"{"kind":"cli_worker"}"# },
            Some(1), false, 10,
        )
        .unwrap();
    assert!(matches!(a, Admission::Queued { .. }));
    let mut forged_running = session("sneaky2", None, SessionStatus::Working, 1);
    forged_running.worker_type = Some("Orchestrator".into());
    store.upsert_session(&forged_running).unwrap();
    assert_eq!(store.running_session_count(None).unwrap(), 2);
}

#[test]
fn claim_queued_starts_legacy_queued_orchestrators_regardless_of_free_slots() {
    let iso = isolate();
    let store = store_in(&iso);
    running(&store, "w1", None);
    admit(&store, "w2", Some(1), false).unwrap();
    let s = spawn_req("o9");
    rusqlite::Connection::open(iso.root().join("ninox.db"))
        .unwrap()
        .execute(
            "INSERT INTO session_queue (id, location, state, kind, enqueued_at, session, payload)
             VALUES ('o9', '', 'queued', 'spawn', 11, ?1, '{\"kind\":\"orchestrator\"}')",
            [serde_json::to_string(&s).unwrap()],
        )
        .unwrap();
    let ids: Vec<String> = store.claim_queued(None, Some(1), 12).unwrap().into_iter().map(|e| e.id).collect();
    assert!(ids.contains(&"o9".to_string()) && !ids.contains(&"w2".to_string()), "{ids:?}");
}

#[test]
fn headline_names_workers_and_orchestrators() {
    let st = LocationStatus {
        machine_id: None, label: "local".into(), running: 28, orchestrators: 15, limit: Some(10), queued: 1,
    };
    assert_eq!(st.headline(), "28/10 workers running locally · 15 orchestrators · 1 queued");
    let one = LocationStatus { orchestrators: 1, queued: 0, ..st.clone() };
    assert_eq!(one.headline(), "28/10 workers running locally · 1 orchestrator");
}
