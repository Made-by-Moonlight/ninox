use super::*;
use ninox_core::ghost_plan::{Proposal, ProposalSection, USER};
use ninox_core::store::QueueState;
use ninox_core::test_isolation::{isolate, Isolation};
use ninox_core::types::{Orchestrator, SessionStatus};
use ninox_core::worker_type::PROMPT_SESSION_ID;

fn store_in(iso: &Isolation) -> Arc<Store> {
    Arc::new(Store::open(iso.root().join("t.db")).unwrap())
}

fn orchestrator_row(store: &Store, id: &str) {
    store.upsert_orchestrator(&Orchestrator { id: id.into(), name: id.into(), created_at: 1 }).unwrap();
    let mut s = crate::session_queue::base_session(id, id, &AppConfig::default().orchestrator, 1);
    s.worker_type = Some("Orchestrator".into());
    store.upsert_session(&s).unwrap();
}

fn busy_worker(store: &Store, id: &str) {
    let mut s = crate::session_queue::base_session(id, id, &AppConfig::default().worker, 1);
    s.status = SessionStatus::Working;
    store.upsert_session(&s).unwrap();
}

/// One running worker per slot: every spawn queues instead of launching.
fn limit(n: u32) {
    AppConfig::update(|c| c.limits.set_limit(None, Some(n))).unwrap();
}

fn section(id: &str, parent: Option<&str>, ty: &str, deps: &[&str], workspace: &str) -> ProposalSection {
    ProposalSection {
        id: id.into(),
        parent_id: parent.map(str::to_string),
        worker_type: Some(ty.into()),
        name: None,
        brief: format!("brief for {id}"),
        dependencies: deps.iter().map(|d| d.to_string()).collect(),
        await_done: false,
        workspace: (ty != "Orchestrator").then(|| workspace.to_string()),
        model: None,
        brain_reads: Vec::new(),
        brain_write: None,
        linear: Some("MLOPS-5309".into()),
    }
}

fn propose(store: &Store, proposal: &Proposal, proposer: &str) -> String {
    let config = AppConfig::load_or_default();
    let v = ghost_plan::prepare_proposal(store, &config, proposal, None).unwrap();
    let id = ghost_plan::new_plan_id(now_ms());
    store.insert_ghost_plan(&id, proposer, &v, 100, 100 + config.plan_proposals.expiry_ms()).unwrap();
    id
}

fn outcome<'a>(report: &'a DecisionReport, node: &str) -> &'a NodeOutcome {
    &report.nodes.iter().find(|n| n.node_id == node).unwrap().outcome
}

#[tokio::test]
async fn the_prompt_agent_is_refused_and_nothing_spawns() {
    let iso = isolate();
    let store = store_in(&iso);
    let p = Proposal {
        parent_id: None,
        title: None,
        sections: vec![section("alpha", None, "Orchestrator", &[], "")],
    };
    let id = propose(&store, &p, PROMPT_SESSION_ID);
    let config = AppConfig::load_or_default();
    let err = accept(&store, &config, &id, None, Some(PROMPT_SESSION_ID), 200).await.unwrap_err().to_string();
    assert!(err.contains("never accept"), "{err}");
    assert!(discard(&store, &id, None, Some(PROMPT_SESSION_ID), 200).is_err());
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    assert!(plan.nodes.iter().all(|n| n.state == NodeState::Ghost));
    assert!(store.list_queue().unwrap().is_empty() && store.get_session("alpha").unwrap().is_none());
}

#[tokio::test]
async fn user_accept_goes_through_the_normal_spawn_path_and_respects_the_limit() {
    let iso = isolate();
    let store = store_in(&iso);
    orchestrator_row(&store, "busy-orch");
    busy_worker(&store, "busy");
    limit(1);
    let repo = tempfile::tempdir().unwrap();
    let ws = repo.path().to_str().unwrap();
    let p = Proposal {
        parent_id: Some("busy-orch".into()),
        title: Some("Q4 objectives".into()),
        sections: vec![
            section("alpha", None, "Implementor", &[], ws),
            section("beta", None, "Implementor", &[], ws),
            section("gamma", None, "Implementor", &[], ws),
        ],
    };
    let id = propose(&store, &p, "busy-orch");
    let config = AppConfig::load_or_default();

    let report = accept(&store, &config, &id, Some(&["alpha".into()]), None, 200).await.unwrap();
    assert_eq!(report.decided_by, USER);
    assert!(matches!(outcome(&report, "alpha"), NodeOutcome::Queued { session_id, .. } if session_id == "alpha"));
    let entry = store.queue_entry("alpha").unwrap().expect("queued under the limit");
    assert_eq!(entry.state, QueueState::Queued);
    let payload: crate::session_queue::QueuedLaunch = serde_json::from_str(&entry.payload).unwrap();
    assert!(matches!(payload, crate::session_queue::QueuedLaunch::CliWorker(w) if w.id == "alpha" && w.orchestrator_id.as_deref() == Some("busy-orch")));
    assert_eq!(store.get_ghost_plan(&id).unwrap().unwrap().plan.status, PlanStatus::Pending, "beta and gamma still undecided");

    // Idempotent: a second accept of the whole plan never re-spawns alpha.
    let again = accept(&store, &config, &id, Some(&["alpha".into()]), None, 300).await.unwrap();
    assert_eq!(outcome(&again, "alpha"), &NodeOutcome::Unchanged { state: NodeState::Spawned });
    assert_eq!(store.list_queue().unwrap().len(), 1);

    let rest = discard(&store, &id, None, None, 400).unwrap();
    assert_eq!(outcome(&rest, "beta"), &NodeOutcome::Discarded);
    assert_eq!(outcome(&rest, "alpha"), &NodeOutcome::Unchanged { state: NodeState::Spawned });
    assert_eq!(rest.status, PlanStatus::Accepted);
    assert!(accept(&store, &config, &id, Some(&["beta".into()]), None, 500).await.unwrap().nodes.iter()
        .all(|n| matches!(n.outcome, NodeOutcome::Unchanged { .. })), "a discarded node can't be accepted later");
}

#[tokio::test]
async fn dependents_wait_and_spawn_in_order() {
    let iso = isolate();
    let store = store_in(&iso);
    orchestrator_row(&store, "lead");
    busy_worker(&store, "busy");
    limit(1);
    let repo = tempfile::tempdir().unwrap();
    let ws = repo.path().to_str().unwrap();
    let p = Proposal {
        parent_id: Some("lead".into()),
        title: None,
        sections: vec![
            section("map", None, "Researcher", &[], ws),
            section("port", None, "Implementor", &["map"], ws),
        ],
    };
    let id = propose(&store, &p, "lead");
    let config = AppConfig::load_or_default();
    let report = accept(&store, &config, &id, None, None, 200).await.unwrap();
    assert!(matches!(outcome(&report, "map"), NodeOutcome::Queued { .. }), "{report:?}");
    assert!(matches!(outcome(&report, "port"), NodeOutcome::Waiting { reason } if reason.contains("map")));
    assert!(store.queue_entry("port").unwrap().is_none(), "a dependent never queues before its dependency starts");

    // `map` starts (as the queue drain would): `port` is next in line.
    let entry = store.queue_entry("map").unwrap().unwrap();
    store.drop_queue_entry("map").unwrap();
    let mut row = entry.session.clone();
    row.status = SessionStatus::Working;
    store.upsert_session(&row).unwrap();
    assert_eq!(row.orchestrator_id.as_deref(), Some("lead"));
    assert_eq!(row.worker_type.as_deref(), Some("Researcher"));

    advance_all(&store, &config, 300).await;
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    assert_eq!(plan.node("port").unwrap().state, NodeState::Spawned);
    assert!(store.queue_entry("port").unwrap().is_some(), "queued: busy + map are over the limit of 1");
}

#[tokio::test]
async fn accepting_a_child_without_its_ghost_parent_is_refused() {
    let iso = isolate();
    let store = store_in(&iso);
    let p = Proposal {
        parent_id: None,
        title: None,
        sections: vec![section("lead", None, "Orchestrator", &[], ""), section("w", Some("lead"), "Implementor", &[], "/r")],
    };
    let id = propose(&store, &p, USER);
    let err = accept(&store, &AppConfig::load_or_default(), &id, Some(&["w".into()]), None, 200).await.unwrap_err().to_string();
    assert!(err.contains("accept it too"), "{err}");
}

#[tokio::test]
async fn an_expired_plan_cannot_be_accepted() {
    let iso = isolate();
    let store = store_in(&iso);
    let p = Proposal { parent_id: None, title: None, sections: vec![section("alpha", None, "Orchestrator", &[], "")] };
    let id = propose(&store, &p, USER);
    let config = AppConfig::load_or_default();
    let err = accept(&store, &config, &id, None, None, 100 + config.plan_proposals.expiry_ms()).await.unwrap_err().to_string();
    assert!(err.contains("expired"), "{err}");
    assert_eq!(store.get_ghost_plan(&id).unwrap().unwrap().plan.status, PlanStatus::Expired);
    assert!(store.list_queue().unwrap().is_empty());
}

/// End to end on the isolated test tmux server with a stand-in harness:
/// an accepted worker node really launches, under its parent, with the
/// node's brief.
#[tokio::test]
async fn an_accepted_worker_node_launches() {
    let iso = isolate();
    let store = store_in(&iso);
    orchestrator_row(&store, "lead");
    AppConfig::update(|c| {
        c.harnesses.insert("plan-fake".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            worker_args: Some(vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{prompt}".into()]),
            ..Default::default()
        });
        c.worker_types.push(ninox_core::worker_type::WorkerTypeSpec {
            name: "FakeWorker".into(),
            harness: "plan-fake".into(),
            ..Default::default()
        });
        c.set_send_mechanism(ninox_core::config::SendMechanism::Keystrokes);
    })
    .unwrap();
    let repo = tempfile::tempdir().unwrap();
    let node = format!("ghost-e2e-{}", std::process::id());
    let p = Proposal {
        parent_id: Some("lead".into()),
        title: None,
        sections: vec![section(&node, None, "FakeWorker", &[], repo.path().to_str().unwrap())],
    };
    let id = propose(&store, &p, "lead");
    let report = accept(&store, &AppConfig::load_or_default(), &id, None, None, 200).await.unwrap();
    let row = store.get_session(&node).unwrap();
    let _ = ninox_core::tmux::kill_session(&node).await;
    assert!(matches!(outcome(&report, &node), NodeOutcome::Spawned { session_id } if session_id == &node), "{report:?}");
    let row = row.expect("session row");
    assert_eq!(row.orchestrator_id.as_deref(), Some("lead"));
    assert_eq!(row.worker_type.as_deref(), Some("FakeWorker"));
    assert!(row.brain_write.is_some());
}

#[test]
fn the_brief_carries_workspace_linear_and_provenance() {
    let iso = isolate();
    let store = store_in(&iso);
    let p = Proposal { parent_id: None, title: None, sections: vec![ProposalSection {
        workspace: Some("/repo/x".into()),
        ..section("alpha", None, "Orchestrator", &[], "")
    }] };
    let id = propose(&store, &p, USER);
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    let brief = node_brief(&plan, plan.node("alpha").unwrap());
    assert!(brief.starts_with("brief for alpha"));
    assert!(brief.contains("Workspace: /repo/x") && brief.contains("Linear: MLOPS-5309") && brief.contains(&id));
}

#[test]
fn a_nodes_spawn_warnings_reach_the_json_report_only_when_there_are_some() {
    let report = |warnings: Vec<String>| {
        serde_json::to_value(NodeReport { node_id: "n1".into(), outcome: NodeOutcome::Spawned { session_id: "w1".into() }, warnings }).unwrap()
    };
    assert!(report(Vec::new()).get("warnings").is_none());
    assert_eq!(report(vec!["w1 is still starting up — sent the brief anyway".into()])["warnings"][0], "w1 is still starting up — sent the brief anyway");
}
