//
// Copyright (c) 2024 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use async_trait::async_trait;
use prost::Message;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use uprotocol_sdk::{
    rpc::{RpcClient, RpcClientResult, RpcMapperError, RpcServer},
    transport::{datamodel::UTransport, validator::Validators},
    uprotocol::{
        Data, UAttributes, UCode, UEntity, UMessage, UMessageType, UPayload, UPayloadFormat,
        UStatus, UUri, Uuid,
    },
    uri::{
        serializer::{LongUriSerializer, UriSerializer},
        validator::UriValidator,
    },
};
use uprotocol_sdk::uprotocol::Remote;
use zenoh::runtime::Runtime;
use zenoh::{
    config::Config,
    prelude::{r#async::*, Sample},
    queryable::{Query, Queryable},
    sample::AttachmentBuilder,
    subscriber::Subscriber,
};

pub struct ZenohListener {}
pub struct ULinkZenoh {
    session: Arc<Session>,
    subscriber_map: Arc<Mutex<HashMap<String, Subscriber<'static, ()>>>>,
    queryable_map: Arc<Mutex<HashMap<String, Queryable<'static, ()>>>>,
    query_map: Arc<Mutex<HashMap<String, Query>>>,
    callback_counter: AtomicU64,
}

impl ULinkZenoh {
    /// # Errors
    /// Will return `Err` if unable to create Zenoh session
    pub async fn new_from_config(config: Config) -> Result<ULinkZenoh, UStatus> {
        let Ok(session) = zenoh::open(config).res().await else {
            return Err(UStatus::fail_with_code(
                UCode::Internal,
                "Unable to open Zenoh session from config",
            ));
        };
        Ok(ULinkZenoh::new(session))
    }

    pub async fn new_from_runtime(runtime: Runtime) -> Result<ULinkZenoh, UStatus> {
        // create a zenoh Session that shares the same Runtime as zenohd
        let Ok(session) = zenoh::init(runtime).res().await else {
            return Err(UStatus::fail_with_code(
                UCode::Internal,
                "Unable to open Zenoh session from runtime",
            ));
        };
        Ok(ULinkZenoh::new(session))
    }

    fn new(session: Session) -> ULinkZenoh {
        ULinkZenoh {
            session: Arc::new(session),
            subscriber_map: Arc::new(Mutex::new(HashMap::new())),
            queryable_map: Arc::new(Mutex::new(HashMap::new())),
            query_map: Arc::new(Mutex::new(HashMap::new())),
            callback_counter: AtomicU64::new(0),
        }
    }

    pub fn to_zenoh_key_string(uri: &UUri) -> Result<String, UStatus> {
        // uProtocol Uri format: https://github.com/eclipse-uprotocol/uprotocol-spec/blob/6f0bb13356c0a377013bdd3342283152647efbf9/basics/uri.adoc#11-rfc3986
        // up://<user@><device>.<domain><:port>/<ue_name>/<ue_version>/<resource|rpc.method><#message>
        //            UAuthority               /        UEntity       /           UResource
        let Ok(mut uri_string) = LongUriSerializer::serialize(uri) else {
            return Err(UStatus::fail_with_code(
                UCode::Internal,
                "Unable to transform to Zenoh key",
            ));
        };

        println!("uri_string: {}", &uri_string);

        if uri_string.starts_with('/') {
            let _ = uri_string.remove(0);
        }

        // TODO: Check whether these characters are all used in UUri.
        // TODO: We should have the # and ? in the attachment instead of Zenoh key
        let mut zenoh_key = uri_string
            .replace('*', "\\8")
            .replace('$', "\\4")
            .replace('?', "\\0")
            .replace('#', "\\3")
            .replace("//", "\\/");

        // Step 2: Check if the authority is a remote name with value "*"
        // Step 1: Check if topic.authority exists
        let authority_exists = uri.authority.is_some();

        // println!("authority_exists: {:?}", authority_exists);

        let is_star_remote = if authority_exists {
            // Step 2: Extract the authority reference
            let authority_ref = uri.authority.as_ref().unwrap(); // safe unwrap because we know it exists

            // println!("authority_ref: {:?}", authority_ref);

            // Step 3: Check if remote is a reference and exists
            let remote_exists = authority_ref.remote.as_ref().is_some();

            // println!("remote_exists: {:?}", remote_exists);

            if remote_exists {
                // Step 4: Extract the remote reference
                let remote_ref = authority_ref.remote.as_ref().unwrap(); // safe unwrap because we know it exists

                // println!("remote_ref: {:?}", remote_ref);

                // Step 5: Check if the remote is a Name type with value "*"
                matches!(remote_ref, Remote::Name(name) if name == "*")
            } else {
                // Remote does not exist
                false
            }
        } else {
            // Authority does not exist
            false
        };

        // println!("is_star_remote: {:?}", is_star_remote);

        // Step 3: Determine the final Zenoh key
        let mut zenoh_key = if is_star_remote {
            "**".to_string()
        } else {
            zenoh_key.clone()
        };

        zenoh_key = "up/".to_owned() + &*zenoh_key;

        println!("zenoh_key: {:?}", zenoh_key);

        Ok(zenoh_key)
    }

    // TODO: We need a standard way in uprotocol-rust to change UUID to String
    fn uuid_to_string(uuid: &Uuid) -> String {
        format!("{}:{}", uuid.msb, uuid.lsb)
    }

    async fn send_publish(
        &self,
        zenoh_key: &str,
        topic: &UUri,
        payload: UPayload,
        attributes: UAttributes,
    ) -> Result<(), UStatus> {
        // Get the data from UPayload
        let Some(Data::Value(buf)) = payload.data else {
            // TODO: Assume we only have Value here, no reference for shared memory
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Invalid data",
            ));
        };

        // Serialized UAttributes into protobuf
        // TODO: Should we map priority into Zenoh priority?
        let mut attr = vec![];
        let Ok(()) = attributes.encode(&mut attr) else {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Unable to encode UAttributes",
            ));
        };

        // Serialized source UUri into protobuf
        let mut src_uuri = vec![];
        let Ok(()) = topic.encode(&mut src_uuri) else {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Unable to encode topic UUri",
            ));
        };

        // Add attachment and payload
        let mut attachment = AttachmentBuilder::new();
        attachment.insert("uattributes", attr.as_slice());
        attachment.insert("src_uuri", src_uuri.as_slice());
        let putbuilder = self
            .session
            .put(zenoh_key, buf)
            .encoding(Encoding::WithSuffix(
                KnownEncoding::AppCustom,
                payload.format.to_string().into(),
            ))
            .with_attachment(attachment.build());

        // println!("ULinkZenoh::send_publish(): before putbuilder.res().await.map_err()");

        // Send data
        putbuilder
            .res()
            .await
            .map_err(|_| UStatus::fail_with_code(UCode::Internal, "Unable to send with Zenoh"))?;

        // println!("ULinkZenoh::send_publish(): after putbuilder.res().await.map_err()");

        Ok(())
    }

    async fn send_response(
        &self,
        zenoh_key: &str,
        payload: UPayload,
        attributes: UAttributes,
    ) -> Result<(), UStatus> {
        // Get the data from UPayload
        let Some(Data::Value(buf)) = payload.data else {
            // TODO: Assume we only have Value here, no reference for shared memory
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Invalid data",
            ));
        };

        // Serialized UAttributes into protobuf
        // TODO: Should we map priority into Zenoh priority?
        let mut attr = vec![];
        let Ok(()) = attributes.encode(&mut attr) else {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Unable to encode UAttributes",
            ));
        };
        // Get reqid
        let reqid = ULinkZenoh::uuid_to_string(&attributes.reqid.ok_or(
            UStatus::fail_with_code(UCode::InvalidArgument, "reqid doesn't exist"),
        )?);

        // Add attachment and payload
        let mut attachment = AttachmentBuilder::new();
        attachment.insert("uattributes", attr.as_slice());
        // Send back query
        let value = Value::new(buf.into()).encoding(Encoding::WithSuffix(
            KnownEncoding::AppCustom,
            payload.format.to_string().into(),
        ));
        let reply = Ok(Sample::new(
            KeyExpr::new(zenoh_key.to_string()).map_err(|_| {
                UStatus::fail_with_code(UCode::Internal, "Unable to create Zenoh key")
            })?,
            value,
        ));
        let query = self
            .query_map
            .lock()
            .unwrap()
            .get(&reqid)
            .ok_or(UStatus::fail_with_code(
                UCode::Internal,
                "query doesn't exist",
            ))?
            .clone();

        // Send data
        // TODO: Unable to use unwrap in with_attachment (Attachment doesn't have Debug trait)
        query
            .reply(reply)
            .with_attachment(attachment.build())
            .map_err(|_| UStatus::fail_with_code(UCode::Internal, "Unable to add attachment"))?
            .res()
            .await
            .map_err(|_| UStatus::fail_with_code(UCode::Internal, "Unable to reply with Zenoh"))?;

        Ok(())
    }
}

#[async_trait]
impl RpcClient for ULinkZenoh {
    async fn invoke_method(
        &self,
        topic: UUri,
        payload: UPayload,
        attributes: UAttributes,
    ) -> RpcClientResult {
        // Validate UUri
        // TODO: Whether we should check URI is resolved or not
        //       https://github.com/eclipse-uprotocol/uprotocol-spec/issues/42#issuecomment-1882273981
        UriValidator::validate(&topic)
            .map_err(|_| RpcMapperError::UnexpectedError(String::from("Wrong UUri")))?;

        // Validate UAttributes
        {
            // TODO: Check why the validator doesn't have Send
            let validator = Validators::Request.validator();
            if let Err(e) = validator.validate(&attributes) {
                return Err(RpcMapperError::UnexpectedError(format!(
                    "Wrong UAttributes {e:?}",
                )));
            }
        }

        // TODO: Speak with Steven about this -- I think here we should create the queryable against the sink
        //  contained in UAttributes

        // Get Zenoh key
        let Ok(zenoh_key) = ULinkZenoh::to_zenoh_key_string(&topic) else {
            return Err(RpcMapperError::UnexpectedError(String::from(
                "Unable to transform to Zenoh key",
            )));
        };

        // Get the data from UPayload
        let Some(Data::Value(buf)) = payload.data else {
            // TODO: Assume we only have Value here, no reference for shared memory
            return Err(RpcMapperError::InvalidPayload(String::from(
                "Wrong UPayload",
            )));
        };

        // Serialized UAttributes into protobuf
        // TODO: Should we map priority into Zenoh priority?
        let mut attr = vec![];
        let Ok(()) = attributes.encode(&mut attr) else {
            return Err(RpcMapperError::ProtobufError(String::from(
                "Unable to encode UAttributes",
            )));
        };

        // Serialized source UUri into protobuf
        let mut src_uuri = vec![];
        let Ok(()) = topic.encode(&mut src_uuri) else {
            return Err(RpcMapperError::UnexpectedError(String::from(
                "Unable to serialize source UUri"
            )));
        };

        // Add attachment and payload
        let mut attachment = AttachmentBuilder::new();
        attachment.insert("uattributes", attr.as_slice());
	    attachment.insert("src_uuri", src_uuri.as_slice());
        let value = Value::new(buf.into()).encoding(Encoding::WithSuffix(
            KnownEncoding::AppCustom,
            payload.format.to_string().into(),
        ));
        // TODO: Query should support .encoding
        // TODO: Adjust the timeout
        let getbuilder = self
            .session
            .get(&zenoh_key)
            .with_value(value)
            .with_attachment(attachment.build())
            .target(QueryTarget::BestMatching)
            .timeout(Duration::from_millis(2000));

        // Send the query
        let Ok(replies) = getbuilder.res().await else {
            return Err(RpcMapperError::UnexpectedError(String::from(
                "Error while sending Zenoh query",
            )));
        };

        let reply = match replies.recv_async().await {
            Ok(reply) => reply,
            Err(e) => {
                // Then return a custom error
                return Err(RpcMapperError::UnexpectedError(format!(
                    "Error while receiving Zenoh reply: {:?}",
                    e
                )));
            }
        };

        // let Ok(reply) = replies.recv_async().await else {
        //     return Err(RpcMapperError::UnexpectedError(String::from(
        //         "Error while receiving Zenoh reply",
        //     )));
        // };
        match reply.sample {
            Ok(sample) => {
                let Ok(encoding) = sample.value.encoding.suffix().parse::<i32>() else {
                    return Err(RpcMapperError::UnexpectedError(String::from(
                        "Error while parsing Zenoh encoding",
                    )));
                };
                Ok(UPayload {
                    length: Some(0),
                    format: encoding,
                    data: Some(Data::Value(sample.payload.contiguous().to_vec())),
                })
            }
            Err(e) => Err(RpcMapperError::UnexpectedError(String::from(
                format!("Error while parsing Zenoh reply: {:?}", e),
            ))),
        }
    }
}

#[async_trait]
impl RpcServer for ULinkZenoh {
    async fn register_rpc_listener(
        &self,
        method: UUri,
        listener: Box<dyn Fn(Result<UMessage, UStatus>) + Send + Sync + 'static>,
    ) -> Result<String, UStatus> {
        // Do the validation
        // TODO: Whether we should check URI is resolved or not
        //       https://github.com/eclipse-uprotocol/uprotocol-spec/issues/42#issuecomment-1882273981
        UriValidator::validate(&method)
            .map_err(|_| UStatus::fail_with_code(UCode::InvalidArgument, "Invalid topic"))?;

        // Get Zenoh key
        let zenoh_key = ULinkZenoh::to_zenoh_key_string(&method)?;
        // Generate listener string for users to delete
        let hashmap_key = format!(
            "{}_{:X}",
            zenoh_key,
            self.callback_counter.fetch_add(1, Ordering::SeqCst)
        );

        let query_map = self.query_map.clone();
        // Setup callback
        let callback = move |query: Query| {
            // Create UAttribute
            let Some(attachment) = query.attachment() else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to get attachment",
                )));
                return;
            };
            let Some(attribute) = attachment.get(&"uattributes".as_bytes()) else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to get uattributes",
                )));
                return;
            };
            let u_attribute: UAttributes = if let Ok(attr) = Message::decode(&*attribute) {
                attr
            } else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to decode attribute",
                )));
                return;
            };
            // Create UPayload
            let u_payload = match query.value() {
                Some(value) => {
                    let Ok(encoding) = value.encoding.suffix().parse::<i32>() else {
                        listener(Err(UStatus::fail_with_code(
                            UCode::Internal,
                            "Unable to get payload encoding",
                        )));
                        return;
                    };
                    UPayload {
                        length: Some(0),
                        format: encoding,
                        data: Some(Data::Value(value.payload.contiguous().to_vec())),
                    }
                }
                None => UPayload {
                    length: Some(0),
                    format: UPayloadFormat::UpayloadFormatUnspecified as i32,
                    data: None,
                },
            };
            // Create UMessage
            let msg = UMessage {
                source: Some(method.clone()),
                attributes: Some(u_attribute.clone()),
                payload: Some(u_payload),
            };
            if let Some(reqid) = u_attribute.reqid {
                query_map
                    .lock()
                    .unwrap()
                    .insert(ULinkZenoh::uuid_to_string(&reqid), query);
            } else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "The request is without reqid in UAttributes",
                )));
                return;
            }
            listener(Ok(msg));
        };
        if let Ok(queryable) = self
            .session
            .declare_queryable(&zenoh_key)
            .callback_mut(callback)
            .res()
            .await
        {
            self.queryable_map
                .lock()
                .unwrap()
                .insert(hashmap_key.clone(), queryable);
        } else {
            return Err(UStatus::fail_with_code(
                UCode::Internal,
                "Unable to register callback with Zenoh",
            ));
        }

        Ok(hashmap_key)
    }
    async fn unregister_rpc_listener(&self, method: UUri, listener: &str) -> Result<(), UStatus> {
        // Do the validation
        // TODO: Whether we should check URI is resolved or not
        //       https://github.com/eclipse-uprotocol/uprotocol-spec/issues/42#issuecomment-1882273981
        UriValidator::validate(&method)
            .map_err(|_| UStatus::fail_with_code(UCode::InvalidArgument, "Invalid topic"))?;
        // TODO: Check whether we still need method or not (Compare method with listener?)

        if self
            .queryable_map
            .lock()
            .unwrap()
            .remove(listener)
            .is_none()
        {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Listener doesn't exist",
            ));
        }

        Ok(())
    }
}

#[async_trait]
impl UTransport for ULinkZenoh {
    async fn authenticate(&self, _entity: UEntity) -> Result<(), UStatus> {
        // TODO: Not implemented
        Err(UStatus::fail_with_code(
            UCode::Unimplemented,
            "Not implemented",
        ))
    }

    async fn send(
        &self,
        topic: UUri,
        payload: UPayload,
        attributes: UAttributes,
    ) -> Result<(), UStatus> {
        // Do the validation
        // TODO: Whether we should check URI is resolved or not
        //       https://github.com/eclipse-uprotocol/uprotocol-spec/issues/42#issuecomment-1882273981
        UriValidator::validate(&topic)
            .map_err(|_| UStatus::fail_with_code(UCode::InvalidArgument, "Invalid topic"))?;
        // TODO: Validate UAttributes (We don't know whether attributes are Publish/Request/Response, so we can't check)

        // Get Zenoh key
        let zenoh_key = ULinkZenoh::to_zenoh_key_string(&topic)?;

        // println!("ULinkZenoh::send(): zenoh_key: {}", &zenoh_key);

        // Check the type of UAttributes (Publish / Request / Response)
        match UMessageType::try_from(attributes.r#type) {
            Ok(UMessageType::UmessageTypePublish) => {
                self.send_publish(&zenoh_key, &topic, payload, attributes).await
            }
            Ok(UMessageType::UmessageTypeResponse) => {
                self.send_response(&zenoh_key, payload, attributes).await
            }
            _ => Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Wrong Message type in UAttributes",
            )),
        }
    }

    async fn register_listener(
        &self,
        topic: UUri,
        listener: Box<dyn Fn(Result<UMessage, UStatus>) + Send + Sync + 'static>,
    ) -> Result<String, UStatus> {
        // Do the validation
        // TODO: Whether we should check URI is resolved or not
        //       https://github.com/eclipse-uprotocol/uprotocol-spec/issues/42#issuecomment-1882273981
        UriValidator::validate(&topic)
            .map_err(|_| UStatus::fail_with_code(UCode::InvalidArgument, "Invalid topic"))?;

        // Get Zenoh key
        let zenoh_key = ULinkZenoh::to_zenoh_key_string(&topic)?;
        // Generate listener string for users to delete
        let hashmap_key = format!(
            "{}_{:X}",
            zenoh_key,
            self.callback_counter.fetch_add(1, Ordering::SeqCst)
        );

        // Setup callback
        let callback = move |sample: Sample| {
            // Create UAttribute
            let Some(attachment) = sample.attachment() else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to get attachment",
                )));
                return;
            };
            let Some(attribute) = attachment.get(&"uattributes".as_bytes()) else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to get uattributes",
                )));
                return;
            };
            let Ok(u_attribute) = Message::decode(&*attribute) else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to decode attribute",
                )));
                return;
            };
            let Some(src_uuri) = attachment.get(&"src_uuri".as_bytes()) else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to get source_uuri",
                )));
                return;
            };
            let Ok(source) = Message::decode(&*src_uuri) else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to decode source uuri",
                )));
                return;
            };
            // Create UPayload
            let Ok(encoding) = sample.encoding.suffix().parse::<i32>() else {
                listener(Err(UStatus::fail_with_code(
                    UCode::Internal,
                    "Unable to get payload encoding",
                )));
                return;
            };
            let u_payload = UPayload {
                length: Some(0),
                format: encoding,
                data: Some(Data::Value(sample.payload.contiguous().to_vec())),
            };
            // Create UMessage
            let msg = UMessage {
                source: Some(source),
                attributes: Some(u_attribute),
                payload: Some(u_payload),
            };
            listener(Ok(msg));
        };
        if let Ok(subscriber) = self
            .session
            .declare_subscriber(&zenoh_key)
            .callback_mut(callback)
            .res()
            .await
        {
            self.subscriber_map
                .lock()
                .unwrap()
                .insert(hashmap_key.clone(), subscriber);
        } else {
            return Err(UStatus::fail_with_code(
                UCode::Internal,
                "Unable to register callback with Zenoh",
            ));
        }

        Ok(hashmap_key)
    }

    async fn unregister_listener(&self, topic: UUri, listener: &str) -> Result<(), UStatus> {
        // Do the validation
        // TODO: Whether we should check URI is resolved or not
        //       https://github.com/eclipse-uprotocol/uprotocol-spec/issues/42#issuecomment-1882273981
        UriValidator::validate(&topic)
            .map_err(|_| UStatus::fail_with_code(UCode::InvalidArgument, "Invalid topic"))?;
        // TODO: Check whether we still need topic or not (Compare topic with listener?)

        if !self.subscriber_map.lock().unwrap().contains_key(listener) {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Listener doesn't exist",
            ));
        }

        self.subscriber_map.lock().unwrap().remove(listener);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uprotocol_sdk::uprotocol::{UEntity, UResource, UUri};

    #[test]
    fn test_to_zenoh_key_string() {
        // create uuri for test
        let uuri = UUri {
            entity: Some(UEntity {
                name: "body.access".to_string(),
                version_major: Some(1),
                ..Default::default()
            }),
            resource: Some(UResource {
                name: "door".to_string(),
                instance: Some("front_left".to_string()),
                message: Some("Door".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            ULinkZenoh::to_zenoh_key_string(&uuri).unwrap(),
            String::from("body.access/1/door.front_left\\3Door")
        );
    }
}
