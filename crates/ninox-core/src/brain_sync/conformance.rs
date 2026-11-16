//! Trait-level conformance suite: the same checks run against every
//! `RemoteStore` implementor, so the sync engine's own tests don't need to
//! know or care which backend is under them (spec: "Testing").
//!
//! `S3RemoteStore` itself isn't instantiated here — there's no
//! credential-free local S3 double in this codebase (its own test coverage
//! lives in `s3.rs` against the real AWS SDK types it maps errors from).
//! `InMemoryRemoteStore` is the reference implementation these checks were
//! first written against (`store.rs`); `GithubWikiRemoteStore` is checked
//! against the identical assertions using a local bare-repo fixture.

use crate::brain_sync::{
    github_wiki::GithubWikiRemoteStore,
    store::{GetResponse, InMemoryRemoteStore, PutOutcome, RemoteStore},
};
use std::path::Path;
use tempfile::tempdir;

async fn get_missing_is_not_found(store: &dyn RemoteStore) {
    assert!(matches!(store.get("nope", None).await.unwrap(), GetResponse::NotFound));
}

async fn put_then_get_round_trips_with_etag(store: &dyn RemoteStore) {
    let etag = store.put("k", b"v".to_vec()).await.unwrap();
    match store.get("k", None).await.unwrap() {
        GetResponse::Found { bytes, etag: e } => {
            assert_eq!(bytes, b"v");
            assert_eq!(e, etag);
        }
        other => panic!("expected Found, got {other:?}"),
    }
}

async fn conditional_get_returns_not_modified_on_matching_etag(store: &dyn RemoteStore) {
    let etag = store.put("k", b"v".to_vec()).await.unwrap();
    assert!(matches!(store.get("k", Some(&etag)).await.unwrap(), GetResponse::NotModified));
    assert!(matches!(store.get("k", Some("stale")).await.unwrap(), GetResponse::Found { .. }));
}

async fn put_if_match_enforces_cas(store: &dyn RemoteStore) {
    let PutOutcome::Ok { etag: e1 } = store.put_if_match("k", b"v1".to_vec(), None).await.unwrap() else {
        panic!("create-only put should succeed on absent key");
    };
    assert!(matches!(store.put_if_match("k", b"v2".to_vec(), None).await.unwrap(), PutOutcome::PreconditionFailed));
    let PutOutcome::Ok { etag: e2 } = store.put_if_match("k", b"v2".to_vec(), Some(&e1)).await.unwrap() else {
        panic!("matching-etag put should succeed");
    };
    assert_ne!(e1, e2);
    assert!(matches!(store.put_if_match("k", b"v3".to_vec(), Some(&e1)).await.unwrap(), PutOutcome::PreconditionFailed));
}

#[tokio::test]
async fn in_memory_store_honors_the_contract() {
    get_missing_is_not_found(&InMemoryRemoteStore::default()).await;
    put_then_get_round_trips_with_etag(&InMemoryRemoteStore::default()).await;
    conditional_get_returns_not_modified_on_matching_etag(&InMemoryRemoteStore::default()).await;
    put_if_match_enforces_cas(&InMemoryRemoteStore::default()).await;
}

async fn init_bare_wiki_fixture(dir: &Path) {
    tokio::fs::create_dir_all(dir).await.unwrap();
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "--quiet", "--bare", "--initial-branch", "master"])
        .output()
        .await
        .unwrap();
}

async fn fresh_wiki_store(bare: &Path) -> GithubWikiRemoteStore {
    GithubWikiRemoteStore::for_clone_url(bare.to_string_lossy().into_owned(), tempdir().unwrap().path().join("clone"))
        .await
        .unwrap()
}

#[tokio::test]
async fn github_wiki_store_honors_the_contract() {
    let bare = tempdir().unwrap();
    init_bare_wiki_fixture(bare.path()).await;
    get_missing_is_not_found(&fresh_wiki_store(bare.path()).await).await;

    let bare = tempdir().unwrap();
    init_bare_wiki_fixture(bare.path()).await;
    put_then_get_round_trips_with_etag(&fresh_wiki_store(bare.path()).await).await;

    let bare = tempdir().unwrap();
    init_bare_wiki_fixture(bare.path()).await;
    conditional_get_returns_not_modified_on_matching_etag(&fresh_wiki_store(bare.path()).await).await;

    let bare = tempdir().unwrap();
    init_bare_wiki_fixture(bare.path()).await;
    put_if_match_enforces_cas(&fresh_wiki_store(bare.path()).await).await;
}
