//! The real S3-compatible `RemoteStore`. Maps SDK responses onto the
//! trait's conditional semantics; every code path above this file is
//! tested against `InMemoryRemoteStore` instead. Auth is the standard AWS
//! credential chain (env vars, shared profiles, SSO) via `aws-config`.

use crate::brain_sync::{
    config::SyncToml,
    store::{GetResponse, PutOutcome, RemoteStore},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::{get_object::GetObjectError, put_object::PutObjectError};

pub struct S3RemoteStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

impl S3RemoteStore {
    pub async fn from_config(cfg: &SyncToml) -> Result<Self> {
        let (bucket, prefix) = cfg.bucket_and_prefix()?;
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = &cfg.region {
            loader = loader.region(aws_config::Region::new(region.clone()));
        }
        let sdk_config = loader.load().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&sdk_config);
        if let Some(endpoint) = &cfg.endpoint {
            // S3-compatible stores (R2, MinIO) generally need path-style
            // addressing; harmless for AWS when an endpoint is set.
            builder = builder.endpoint_url(endpoint.clone()).force_path_style(true);
        }
        Ok(Self { client: aws_sdk_s3::Client::from_conf(builder.build()), bucket, prefix })
    }

    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }
}

/// True when a `GetObject` SDK error is an HTTP-level status we handle as
/// a conditional-request outcome (304) rather than a failure.
fn get_status(err: &SdkError<GetObjectError>) -> Option<u16> {
    match err {
        SdkError::ServiceError(se) => Some(se.raw().status().as_u16()),
        SdkError::ResponseError(re) => Some(re.raw().status().as_u16()),
        _ => None,
    }
}

/// Same as `get_status`, for `PutObject`'s distinct error type — the SDK
/// gives every operation its own error enum, so there is no shared type to
/// write one generic helper against without fighting extra trait bounds.
fn put_status(err: &SdkError<PutObjectError>) -> Option<u16> {
    match err {
        SdkError::ServiceError(se) => Some(se.raw().status().as_u16()),
        SdkError::ResponseError(re) => Some(re.raw().status().as_u16()),
        _ => None,
    }
}

#[async_trait]
impl RemoteStore for S3RemoteStore {
    async fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse> {
        let mut req = self.client.get_object().bucket(&self.bucket).key(self.full_key(key));
        if let Some(etag) = if_none_match {
            req = req.if_none_match(etag);
        }
        match req.send().await {
            Ok(out) => {
                let etag = out.e_tag().unwrap_or_default().to_string();
                let bytes = out.body.collect().await.context("read object body")?.into_bytes().to_vec();
                Ok(GetResponse::Found { bytes, etag })
            }
            Err(err) => {
                if matches!(get_status(&err), Some(304)) {
                    return Ok(GetResponse::NotModified);
                }
                if let SdkError::ServiceError(se) = &err {
                    if se.err().is_no_such_key() {
                        return Ok(GetResponse::NotFound);
                    }
                }
                if matches!(get_status(&err), Some(404)) {
                    return Ok(GetResponse::NotFound);
                }
                Err(err).with_context(|| format!("get s3://{}/{}", self.bucket, self.full_key(key)))
            }
        }
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
        let out = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .body(bytes.into())
            .send()
            .await
            .with_context(|| format!("put s3://{}/{}", self.bucket, self.full_key(key)))?;
        Ok(out.e_tag().unwrap_or_default().to_string())
    }

    async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> Result<PutOutcome> {
        let mut req = self.client.put_object().bucket(&self.bucket).key(self.full_key(key)).body(bytes.into());
        req = match expected_etag {
            Some(etag) => req.if_match(etag),
            None => req.if_none_match("*"),
        };
        match req.send().await {
            Ok(out) => Ok(PutOutcome::Ok { etag: out.e_tag().unwrap_or_default().to_string() }),
            Err(err) if matches!(put_status(&err), Some(412) | Some(409)) => Ok(PutOutcome::PreconditionFailed),
            Err(err) => Err(err).with_context(|| format!("put s3://{}/{}", self.bucket, self.full_key(key))),
        }
    }
}
