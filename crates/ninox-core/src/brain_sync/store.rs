use anyhow::{bail, Result};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
};

#[derive(Debug)]
pub enum GetResponse {
    Found { bytes: Vec<u8>, etag: String },
    /// Conditional get: the caller's ETag still matches.
    NotModified,
    NotFound,
}

#[derive(Debug)]
pub enum PutOutcome {
    Ok { etag: String },
    /// The compare-and-swap lost: the object changed since `expected_etag`
    /// (or already exists, for create-only puts).
    PreconditionFailed,
}

/// Minimal conditional-request surface of an S3-compatible object store —
/// exactly what the sync engine needs and nothing more. Implemented by
/// `s3::S3RemoteStore` for real use and `InMemoryRemoteStore` for tests.
#[async_trait]
pub trait RemoteStore: Send + Sync {
    /// `if_none_match: Some(etag)` turns this into a conditional get that
    /// answers `NotModified` when the object still has that ETag.
    async fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse>;
    /// Unconditional write; returns the new ETag. Used only for immutable,
    /// hash-keyed entry objects where overwrites are byte-identical.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String>;
    /// Compare-and-swap write. `expected_etag: None` means create-only
    /// (`If-None-Match: *`). Used only for the manifest.
    async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> Result<PutOutcome>;
}

/// Test double with real conditional-request semantics. `fail_all` makes
/// every call error, for offline-fallback tests.
#[derive(Default)]
pub struct InMemoryRemoteStore {
    objects: Mutex<BTreeMap<String, (Vec<u8>, String)>>,
    etag_counter: AtomicU64,
    pub fail_all: AtomicBool,
}

impl InMemoryRemoteStore {
    fn next_etag(&self) -> String {
        format!("e{}", self.etag_counter.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn check_up(&self) -> Result<()> {
        if self.fail_all.load(Ordering::SeqCst) {
            bail!("simulated remote failure");
        }
        Ok(())
    }
}

#[async_trait]
impl RemoteStore for InMemoryRemoteStore {
    async fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse> {
        self.check_up()?;
        let objects = self.objects.lock().unwrap();
        match objects.get(key) {
            None => Ok(GetResponse::NotFound),
            Some((bytes, etag)) => {
                if if_none_match == Some(etag.as_str()) {
                    Ok(GetResponse::NotModified)
                } else {
                    Ok(GetResponse::Found { bytes: bytes.clone(), etag: etag.clone() })
                }
            }
        }
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
        self.check_up()?;
        let etag = self.next_etag();
        self.objects.lock().unwrap().insert(key.to_string(), (bytes, etag.clone()));
        Ok(etag)
    }

    async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> Result<PutOutcome> {
        self.check_up()?;
        let mut objects = self.objects.lock().unwrap();
        let current = objects.get(key).map(|(_, e)| e.as_str());
        let ok = match (expected_etag, current) {
            (None, None) => true,                       // create-only, absent
            (None, Some(_)) => false,                   // create-only, exists
            (Some(_), None) => false,                   // expected something, gone
            (Some(e), Some(c)) => e == c,
        };
        if !ok {
            return Ok(PutOutcome::PreconditionFailed);
        }
        let etag = self.next_etag();
        objects.insert(key.to_string(), (bytes, etag.clone()));
        Ok(PutOutcome::Ok { etag })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let store = InMemoryRemoteStore::default();
        assert!(matches!(store.get("nope", None).await.unwrap(), GetResponse::NotFound));
    }

    #[tokio::test]
    async fn put_then_get_round_trips_with_etag() {
        let store = InMemoryRemoteStore::default();
        let etag = store.put("k", b"v".to_vec()).await.unwrap();
        match store.get("k", None).await.unwrap() {
            GetResponse::Found { bytes, etag: e } => {
                assert_eq!(bytes, b"v");
                assert_eq!(e, etag);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn conditional_get_returns_not_modified_on_matching_etag() {
        let store = InMemoryRemoteStore::default();
        let etag = store.put("k", b"v".to_vec()).await.unwrap();
        assert!(matches!(store.get("k", Some(&etag)).await.unwrap(), GetResponse::NotModified));
        assert!(matches!(store.get("k", Some("stale")).await.unwrap(), GetResponse::Found { .. }));
    }

    #[tokio::test]
    async fn put_if_match_enforces_cas() {
        let store = InMemoryRemoteStore::default();
        // create-only succeeds when absent, fails when present
        let PutOutcome::Ok { etag: e1 } = store.put_if_match("k", b"v1".to_vec(), None).await.unwrap() else {
            panic!("create-only put should succeed on absent key");
        };
        assert!(matches!(
            store.put_if_match("k", b"v2".to_vec(), None).await.unwrap(),
            PutOutcome::PreconditionFailed
        ));
        // matching etag succeeds, stale etag fails
        let PutOutcome::Ok { etag: e2 } = store.put_if_match("k", b"v2".to_vec(), Some(&e1)).await.unwrap() else {
            panic!("matching-etag put should succeed");
        };
        assert_ne!(e1, e2);
        assert!(matches!(
            store.put_if_match("k", b"v3".to_vec(), Some(&e1)).await.unwrap(),
            PutOutcome::PreconditionFailed
        ));
    }

    #[tokio::test]
    async fn fail_all_makes_every_call_error() {
        let store = InMemoryRemoteStore::default();
        store.fail_all.store(true, Ordering::SeqCst);
        assert!(store.get("k", None).await.is_err());
        assert!(store.put("k", vec![]).await.is_err());
        assert!(store.put_if_match("k", vec![], None).await.is_err());
    }
}
