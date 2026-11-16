use super::*;
use crate::fleet::snapshot::test_support::session;
use crate::store::{NewQueueEntry, QueueKind};
use crate::test_isolation::{isolate, Isolation};
use crate::types::Orchestrator;

fn store_in(iso: &Isolation) -> Store {
    Store::open(iso.root().join("ninox.db")).unwrap()
}

fn orchestrator(store: &Store, id: &str, parent: Option<&str>) {
    store.upsert_orchestrator(&Orchestrator { id: id.into(), name: id.into(), created_at: 1 }).unwrap();
    let mut s = session(id, parent, SessionStatus::Working, 1);
    s.worker_type = Some("Orchestrator".into());
    store.upsert_session(&s).unwrap();
}

fn section(id: &str, parent: Option<&str>, ty: &str, deps: &[&str]) -> ProposalSection {
    ProposalSection {
        id: id.into(),
        parent_id: parent.map(str::to_string),
        worker_type: Some(ty.into()),
        name: None,
        brief: format!("do {id}"),
        dependencies: deps.iter().map(|d| d.to_string()).collect(),
        await_done: false,
        workspace: (ty != "Orchestrator").then(|| "/repo".to_string()),
        model: None,
        brain_reads: Vec::new(),
        brain_write: None,
        linear: None,
    }
}

fn three_orchestrators() -> Proposal {
    Proposal {
        parent_id: None,
        title: Some("Q4".into()),
        sections: vec![
            section("alpha", None, "Orchestrator", &[]),
            section("beta", None, "Orchestrator", &[]),
            section("gamma", None, "Orchestrator", &[]),
        ],
    }
}

fn propose(store: &Store, config: &AppConfig, proposal: &Proposal, proposer: &str, now: i64) -> String {
    let v = prepare_proposal(store, config, proposal, None).unwrap();
    let id = new_plan_id(now);
    store.insert_ghost_plan(&id, proposer, &v, now, now + config.plan_proposals.expiry_ms()).unwrap();
    id
}

fn codes(errors: &[ProposalError]) -> Vec<&'static str> {
    errors.iter().map(|e| e.code).collect()
}

#[test]
fn propose_stores_a_pending_plan_that_lists_and_shows() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    let id = propose(&store, &config, &three_orchestrators(), PROMPT_SESSION_ID, 1_000);

    let pending = store.list_ghost_plans(true).unwrap();
    assert_eq!(pending.len(), 1);
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    assert_eq!(plan.plan.status, PlanStatus::Pending);
    assert_eq!(plan.plan.proposer, PROMPT_SESSION_ID);
    assert_eq!(plan.plan.target_parent, None);
    assert_eq!(plan.plan.expires_at, 1_000 + 7 * 24 * 60 * 60 * 1000);
    let ids: Vec<_> = plan.nodes.iter().map(|n| n.node_id.as_str()).collect();
    assert_eq!(ids, ["alpha", "beta", "gamma"]);
    assert!(plan.nodes.iter().all(|n| n.state == NodeState::Ghost && n.brain.is_some()));
}

#[test]
fn validation_rejects_bad_proposals() {
    let registry = AppConfig::default().worker_type_registry();
    let mut p = three_orchestrators();
    p.sections[1].worker_type = Some("Implementor".into());
    p.sections[2].worker_type = Some("NoSuchType".into());
    p.sections.push(section("w", Some("alpha"), "Implementor", &["w"]));
    p.sections.push(ProposalSection { name: Some("Alpha".into()), ..section("dup", Some("alpha"), "Researcher", &[]) });
    p.sections.push(ProposalSection { workspace: None, ..section("nows", Some("alpha"), "Researcher", &[]) });
    let errors = validate_proposal(&p, &registry, 3, 0);
    let c = codes(&errors);
    for want in ["top_level_not_orchestrator", "unknown_worker_type", "dependency_cycle", "duplicate_name", "missing_workspace"] {
        assert!(c.contains(&want), "expected {want} in {c:?}");
    }
}

#[test]
fn validation_enforces_depth_and_nested_ownership() {
    let registry = AppConfig::default().worker_type_registry();
    let p = Proposal {
        parent_id: Some("root".into()),
        title: None,
        sections: vec![
            section("impl", None, "Implementor", &[]),
            section("child", Some("impl"), "Researcher", &[]),
        ],
    };
    assert!(codes(&validate_proposal(&p, &registry, 3, 0)).contains(&"nested_spawn_not_allowed"));

    let deep = Proposal {
        parent_id: Some("root".into()),
        title: None,
        sections: vec![
            section("o1", None, "Orchestrator", &[]),
            section("o2", Some("o1"), "Orchestrator", &[]),
            section("o3", Some("o2"), "Orchestrator", &[]),
        ],
    };
    assert!(validate_proposal(&deep, &registry, 3, 0).iter().any(|e| e.code == "depth_exceeded"));
    // A new top-level orchestrator is depth 0, so the same three levels fit.
    let top = Proposal { parent_id: None, ..deep };
    assert!(validate_proposal(&top, &registry, 3, 0).is_empty());
}

#[test]
fn prepare_refuses_an_unknown_target_parent() {
    let iso = isolate();
    let store = store_in(&iso);
    let p = Proposal { parent_id: Some("ghost-orch".into()), title: None, sections: vec![section("w", None, "Implementor", &[])] };
    let errors = prepare_proposal(&store, &AppConfig::default(), &p, None).unwrap_err();
    assert_eq!(codes(&errors), ["unknown_parent"]);
}

#[test]
fn the_prompt_agent_and_workers_can_never_decide() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    orchestrator(&store, "ninox", None);
    store.upsert_session(&session("worker", Some("ninox"), SessionStatus::Working, 1)).unwrap();
    let top = propose(&store, &config, &three_orchestrators(), PROMPT_SESSION_ID, 10);
    let plan = store.get_ghost_plan(&top).unwrap().unwrap().plan;

    let err = authorize_decision(&store, &plan, Some(PROMPT_SESSION_ID)).unwrap_err().to_string();
    assert!(err.contains("never accept"), "{err}");
    assert!(authorize_decision(&store, &plan, Some("worker")).is_err());
    assert!(authorize_decision(&store, &plan, Some("ninox")).is_err(), "top-level plans are user-only");
    assert_eq!(authorize_decision(&store, &plan, None).unwrap(), Decider::User);
}

#[test]
fn an_orchestrator_decides_only_plans_under_itself() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    orchestrator(&store, "ninox", None);
    orchestrator(&store, "sub", Some("ninox"));
    orchestrator(&store, "other", None);
    let under_sub = Proposal { parent_id: Some("sub".into()), title: None, sections: vec![section("w", None, "Implementor", &[])] };
    let id = propose(&store, &config, &under_sub, "sub", 10);
    let plan = store.get_ghost_plan(&id).unwrap().unwrap().plan;

    assert_eq!(authorize_decision(&store, &plan, Some("sub")).unwrap(), Decider::Orchestrator("sub".into()));
    assert_eq!(authorize_decision(&store, &plan, Some("ninox")).unwrap(), Decider::Orchestrator("ninox".into()));
    assert!(authorize_decision(&store, &plan, Some("other")).is_err());
}

#[test]
fn readiness_waits_for_parent_and_dependencies() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    orchestrator(&store, "ninox", None);
    let mut done_dep = section("impl", Some("lead"), "Implementor", &["map"]);
    done_dep.await_done = true;
    let p = Proposal {
        parent_id: Some("ninox".into()),
        title: None,
        sections: vec![
            section("lead", None, "Orchestrator", &[]),
            section("map", Some("lead"), "Researcher", &[]),
            done_dep,
        ],
    };
    let id = propose(&store, &config, &p, "ninox", 10);
    let all: Vec<String> = ["lead", "map", "impl"].map(String::from).to_vec();
    store.transition_ghost_nodes(&id, &all, &[NodeState::Ghost], NodeState::Waiting, 11).unwrap();
    let load = || store.get_ghost_plan(&id).unwrap().unwrap();

    let plan = load();
    assert_eq!(readiness(&store, &plan, plan.node("lead").unwrap()).unwrap(), Readiness::Ready { parent_session: Some("ninox".into()) });
    assert!(matches!(readiness(&store, &plan, plan.node("map").unwrap()).unwrap(), Readiness::Wait(_)));

    // The lead is queued under the session limit: still not started.
    let skeleton = session("lead", Some("ninox"), SessionStatus::Working, 1);
    store.admit_session(NewQueueEntry { kind: QueueKind::Spawn, session: &skeleton, payload: "{}" }, Some(0), false, 12).unwrap();
    store.claim_ghost_node(&id, "lead", 12).unwrap();
    store.finish_ghost_node(&id, "lead", NodeState::Spawned, Some("lead"), None, 12).unwrap();
    let plan = load();
    assert!(matches!(readiness(&store, &plan, plan.node("map").unwrap()).unwrap(), Readiness::Wait(r) if r.contains("start")));

    store.drop_queue_entry("lead").unwrap();
    store.upsert_session(&skeleton).unwrap();
    let plan = load();
    assert_eq!(readiness(&store, &plan, plan.node("map").unwrap()).unwrap(), Readiness::Ready { parent_session: Some("lead".into()) });

    store.claim_ghost_node(&id, "map", 13).unwrap();
    store.finish_ghost_node(&id, "map", NodeState::Spawned, Some("map"), None, 13).unwrap();
    store.upsert_session(&session("map", Some("lead"), SessionStatus::Working, 1)).unwrap();
    let plan = load();
    assert!(matches!(readiness(&store, &plan, plan.node("impl").unwrap()).unwrap(), Readiness::Wait(r) if r.contains("finish")));
    store.upsert_session(&session("map", Some("lead"), SessionStatus::Done, 1)).unwrap();
    let plan = load();
    assert!(matches!(readiness(&store, &plan, plan.node("impl").unwrap()).unwrap(), Readiness::Ready { .. }));
}

#[test]
fn a_discarded_dependency_blocks_its_dependents() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    orchestrator(&store, "ninox", None);
    let p = Proposal {
        parent_id: Some("ninox".into()),
        title: None,
        sections: vec![section("a", None, "Researcher", &[]), section("b", None, "Implementor", &["a"])],
    };
    let id = propose(&store, &config, &p, "ninox", 10);
    store.transition_ghost_nodes(&id, &["b".into()], &[NodeState::Ghost], NodeState::Waiting, 11).unwrap();
    store.transition_ghost_nodes(&id, &["a".into()], &[NodeState::Ghost], NodeState::Discarded, 11).unwrap();
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    assert!(matches!(readiness(&store, &plan, plan.node("b").unwrap()).unwrap(), Readiness::Blocked(_)));
    assert_eq!(plan.plan.status, PlanStatus::Accepted);
}

#[test]
fn transitions_are_single_winner_and_settle_the_plan() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    let id = propose(&store, &config, &three_orchestrators(), USER, 10);
    let alpha = vec!["alpha".to_string()];
    assert_eq!(store.transition_ghost_nodes(&id, &alpha, &[NodeState::Ghost], NodeState::Waiting, 11).unwrap(), alpha);
    assert!(store.transition_ghost_nodes(&id, &alpha, &[NodeState::Ghost], NodeState::Waiting, 12).unwrap().is_empty());
    assert!(store.claim_ghost_node(&id, "alpha", 12).unwrap());
    assert!(!store.claim_ghost_node(&id, "alpha", 12).unwrap(), "a second claim must lose");
    assert_eq!(store.get_ghost_plan(&id).unwrap().unwrap().plan.status, PlanStatus::Pending);

    store.transition_ghost_nodes(&id, &["beta".into(), "gamma".into()], &[NodeState::Ghost], NodeState::Discarded, 13).unwrap();
    assert_eq!(store.get_ghost_plan(&id).unwrap().unwrap().plan.status, PlanStatus::Accepted);

    let all_out = propose(&store, &config, &three_orchestrators(), USER, 20);
    let ids: Vec<String> = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    store.transition_ghost_nodes(&all_out, &ids, &[NodeState::Ghost], NodeState::Discarded, 21).unwrap();
    assert_eq!(store.get_ghost_plan(&all_out).unwrap().unwrap().plan.status, PlanStatus::Discarded);
}

#[test]
fn pending_plans_expire_and_are_never_accepted() {
    let iso = isolate();
    let store = store_in(&iso);
    let mut config = AppConfig::default();
    config.plan_proposals.expiry_days = 1;
    let old = propose(&store, &config, &three_orchestrators(), USER, 0);
    let fresh = propose(&store, &config, &three_orchestrators(), USER, 100);
    store.transition_ghost_nodes(&fresh, &["alpha".into()], &[NodeState::Ghost], NodeState::Waiting, 101).unwrap();

    let day = config.plan_proposals.expiry_ms();
    assert_eq!(store.expire_ghost_plans(day).unwrap(), vec![old.clone()]);
    let p = store.get_ghost_plan(&old).unwrap().unwrap();
    assert_eq!(p.plan.status, PlanStatus::Expired);
    assert!(p.nodes.iter().all(|n| n.state == NodeState::Expired));

    store.expire_ghost_plans(day + 100).unwrap();
    let p = store.get_ghost_plan(&fresh).unwrap().unwrap();
    assert_eq!(p.plan.status, PlanStatus::Accepted, "an accepted node survives expiry");
    assert_eq!(p.node("alpha").unwrap().state, NodeState::Waiting);
    assert_eq!(p.node("beta").unwrap().state, NodeState::Expired);
}

#[test]
fn stale_claims_recover() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    let id = propose(&store, &config, &three_orchestrators(), USER, 0);
    let ids: Vec<String> = ["alpha", "beta"].map(String::from).to_vec();
    store.transition_ghost_nodes(&id, &ids, &[NodeState::Ghost], NodeState::Waiting, 0).unwrap();
    store.claim_ghost_node(&id, "alpha", 0).unwrap();
    store.claim_ghost_node(&id, "beta", 0).unwrap();
    store.upsert_session(&session("beta", None, SessionStatus::Working, 1)).unwrap();
    assert_eq!(store.recover_stale_ghost_claims(60 * 60 * 1000).unwrap(), 2);
    let p = store.get_ghost_plan(&id).unwrap().unwrap();
    assert_eq!(p.node("alpha").unwrap().state, NodeState::Waiting);
    assert_eq!(p.node("beta").unwrap().state, NodeState::Failed, "a name already taken is never guessed to be ours");
    assert_eq!(p.node("beta").unwrap().session_id, None);
}

#[test]
fn a_blocked_verdict_never_overrides_a_live_claim() {
    let iso = isolate();
    let store = store_in(&iso);
    let id = propose(&store, &AppConfig::default(), &three_orchestrators(), USER, 0);
    store.transition_ghost_nodes(&id, &["alpha".into()], &[NodeState::Ghost], NodeState::Waiting, 0).unwrap();
    store.claim_ghost_node(&id, "alpha", 1).unwrap();
    assert!(!store.fail_waiting_ghost_node(&id, "alpha", "stale", 2).unwrap());
    assert_eq!(store.get_ghost_plan(&id).unwrap().unwrap().node("alpha").unwrap().state, NodeState::Spawning);
}

#[test]
fn a_vanished_target_or_dependency_blocks_instead_of_waiting_forever() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    orchestrator(&store, "ninox", None);
    let p = Proposal {
        parent_id: Some("ninox".into()),
        title: None,
        sections: vec![section("a", None, "Researcher", &[]), section("b", None, "Implementor", &["a"])],
    };
    let id = propose(&store, &config, &p, "ninox", 10);
    let both: Vec<String> = ["a", "b"].map(String::from).to_vec();
    store.transition_ghost_nodes(&id, &both, &[NodeState::Ghost], NodeState::Waiting, 11).unwrap();
    store.claim_ghost_node(&id, "a", 12).unwrap();
    store.finish_ghost_node(&id, "a", NodeState::Spawned, Some("a"), None, 12).unwrap();
    // `a` was spawned but its row and queue entry are gone (removed, cancelled, purged).
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    assert!(matches!(readiness(&store, &plan, plan.node("b").unwrap()).unwrap(), Readiness::Blocked(r) if r.contains("no longer exists")));

    store.delete_session("ninox").unwrap();
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    let mut fresh = plan.node("b").unwrap().clone();
    fresh.dependencies.clear();
    assert!(matches!(readiness(&store, &plan, &fresh).unwrap(), Readiness::Blocked(r) if r.contains("target")));
}

#[test]
fn await_done_counts_a_merged_or_self_completed_dependency_as_finished() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    orchestrator(&store, "ninox", None);
    let mut port = section("port", None, "Implementor", &["map"]);
    port.await_done = true;
    let p = Proposal { parent_id: Some("ninox".into()), title: None, sections: vec![section("map", None, "Researcher", &[]), port] };
    let id = propose(&store, &config, &p, "ninox", 10);
    store.transition_ghost_nodes(&id, &["map".into(), "port".into()], &[NodeState::Ghost], NodeState::Waiting, 11).unwrap();
    store.claim_ghost_node(&id, "map", 12).unwrap();
    store.finish_ghost_node(&id, "map", NodeState::Spawned, Some("map"), None, 12).unwrap();
    let mut map = session("map", Some("ninox"), SessionStatus::PrOpen, 1);
    store.upsert_session(&map).unwrap();
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    assert!(matches!(readiness(&store, &plan, plan.node("port").unwrap()).unwrap(), Readiness::Wait(_)));
    map.merged_at = Some(20);
    store.upsert_session(&map).unwrap();
    let plan = store.get_ghost_plan(&id).unwrap().unwrap();
    assert!(matches!(readiness(&store, &plan, plan.node("port").unwrap()).unwrap(), Readiness::Ready { .. }));
}

#[test]
fn a_cycle_through_parent_edges_is_rejected() {
    let registry = AppConfig::default().worker_type_registry();
    // A waits for B1, B1 waits for its parent B, B waits for A.
    let p = Proposal {
        parent_id: None,
        title: None,
        sections: vec![
            section("a", None, "Orchestrator", &["b1"]),
            section("b", None, "Orchestrator", &["a"]),
            section("b1", Some("b"), "Researcher", &[]),
        ],
    };
    assert!(codes(&validate_proposal(&p, &registry, 3, 0)).contains(&"dependency_cycle"));
}

#[test]
fn a_session_proposers_nodes_default_to_its_own_brain_set() {
    let iso = isolate();
    let store = store_in(&iso);
    let mut config = AppConfig::default();
    let narrow = iso.root().join("narrow");
    config.brain.path = Some(iso.root().join("default"));
    orchestrator(&store, "ninox", None);
    let mut proposer = session("worker", Some("ninox"), SessionStatus::Working, 1);
    crate::brain_set::BrainSet::single(&narrow).apply_to(&mut proposer);
    store.upsert_session(&proposer).unwrap();
    let p = Proposal { parent_id: Some("ninox".into()), title: None, sections: vec![section("r", None, "Researcher", &[])] };
    let v = prepare_proposal(&store, &config, &p, Some("worker")).unwrap();
    assert_eq!(v.nodes[0].brain.as_ref().unwrap().write, narrow);
}

#[test]
fn turn_window_matches_proposer_and_time() {
    let iso = isolate();
    let store = store_in(&iso);
    let config = AppConfig::default();
    let mine = propose(&store, &config, &three_orchestrators(), PROMPT_SESSION_ID, 150);
    propose(&store, &config, &three_orchestrators(), USER, 160);
    let later = propose(&store, &config, &three_orchestrators(), PROMPT_SESSION_ID, 250);
    let plans = store.list_ghost_plans(false).unwrap();

    let first: Vec<_> = plans_in_window(&plans, PROMPT_SESSION_ID, 100, Some(200)).iter().map(|p| p.plan.id.clone()).collect();
    assert_eq!(first, vec![mine]);
    let last: Vec<_> = plans_in_window(&plans, PROMPT_SESSION_ID, 200, None).iter().map(|p| p.plan.id.clone()).collect();
    assert_eq!(last, vec![later]);
}

#[test]
fn proposal_json_accepts_the_generate_shape_plus_extras() {
    let raw = r#"{"parent_id":"ninox","sections":[
        {"id":"a","parent_id":"ninox","worker_type":"Researcher","brief":"map it","dependencies":[],
         "name":"Map it","workspace":"/r","linear":"MLOPS-1","model":"opus","brain_reads":["default"]}]}"#;
    let p: Proposal = serde_json::from_str(raw).unwrap();
    assert_eq!(p.sections[0].linear.as_deref(), Some("MLOPS-1"));
    assert!(serde_json::from_str::<Proposal>(r#"{"sections":[{"id":"a","brief":"b","worker_type":"X","bogus":1}]}"#).is_err());
}
