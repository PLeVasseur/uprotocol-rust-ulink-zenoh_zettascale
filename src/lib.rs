use async_trait::async_trait;
use uprotocol_sdk::{
    rpc::{RpcClient, RpcClientResult},
    transport::datamodel::UTransport,
    uprotocol::{UAttributes, UEntity, UMessage, UPayload, UStatus, UUri},
};
use zenoh::config::Config;
use zenoh::prelude::r#async::*;

pub struct ZenohListener {}
pub struct ULink {
    _session: Session,
}

impl ULink {
    pub async fn new() -> anyhow::Result<ULink> {
        let session = zenoh::open(Config::default()).res().await.unwrap();
        Ok(ULink { _session: session })
    }
}

#[async_trait]
impl RpcClient for ULink {
    async fn invoke_method(
        _topic: UUri,
        payload: UPayload,
        _attributes: UAttributes,
    ) -> RpcClientResult {
        Ok(payload)
    }
}

#[async_trait]
impl UTransport for ULink {
    async fn authenticate(&self, _entity: UEntity) -> Result<(), UStatus> {
        Err(UStatus::fail("Not implemented"))
    }

    async fn send(
        &self,
        _topic: UUri,
        _payload: UPayload,
        _attributes: UAttributes,
    ) -> Result<(), UStatus> {
        Err(UStatus::fail("Not implemented"))
    }

    async fn register_listener(
        &self,
        _topic: UUri,
        _listener: Box<dyn Fn(UMessage) + Send + 'static>,
    ) -> Result<String, UStatus> {
        Err(UStatus::fail("Not implemented"))
    }

    async fn unregister_listener(&self, _topic: UUri, _listener: &str) -> Result<(), UStatus> {
        Err(UStatus::fail("Not implemented"))
    }
}
