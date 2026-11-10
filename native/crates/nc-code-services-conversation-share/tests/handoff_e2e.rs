//! Handoff e2e for `nc-code-services-conversation-share`: proves the skeleton compiles and its
//! persistence structs round-trip through serde (and, where the crate
//! registers a channel, through the nc-code-rpc wire encoding).
//!
//! This is a pure data/types test: no HTTP, no terminal IO, no
//! websockets, no SQLite driver, no user-directory access.
use nc_code_services_conversation_share::handoff::ServicesConversationSharePort;

#[test]
fn handoff_record_round_trips_through_json() {
    let record =
        nc_code_services_conversation_share::handoff::ShareRecord::new("e2e-1", "e2e-kind");
    let pretty = record.to_json_pretty().expect("pretty");
    let back = nc_code_services_conversation_share::handoff::ShareRecord::from_json(&pretty)
        .expect("parse");
    assert_eq!(back, record);
    // camelCase field names match the channel payloads.
    assert!(pretty.contains("\"createdAt\"") || pretty.contains("\"created_at\""));
    assert_eq!(back.id, "e2e-1");
}

#[test]
fn handoff_envelope_version_is_stable() {
    let envelope = nc_code_services_conversation_share::handoff::HandoffEnvelope::new(Vec::new());
    assert_eq!(
        envelope.version,
        nc_code_services_conversation_share::handoff::HANDOFF_VERSION
    );
    assert_eq!(
        envelope.crate_name,
        nc_code_services_conversation_share::handoff::HANDOFF_CRATE
    );
    let text = serde_json::to_string(&envelope).expect("serialize");
    let back: nc_code_services_conversation_share::handoff::HandoffEnvelope<
        nc_code_services_conversation_share::handoff::ShareRecord,
    > = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back, envelope);
}

#[test]
fn handoff_store_trait_is_send_sync() {
    // The port trait must be Send + Sync so it can live behind Arc<dyn Trait>
    // in channel dispatch.
    fn assert_send<T: Send + Sync>() {}
    assert_send::<nc_code_services_conversation_share::handoff::ShareRecord>();
    // In-memory store satisfies the port trait.
    let store = nc_code_services_conversation_share::handoff::InMemoryHandoffStore::new();
    let _trait_obj: Box<
        dyn nc_code_services_conversation_share::handoff::ServicesConversationSharePort,
    > = Box::new(nc_code_services_conversation_share::handoff::InMemoryHandoffStore::new());
    let record = nc_code_services_conversation_share::handoff::ShareRecord::new("s1", "k1");
    store.upsert(record.clone());
    assert_eq!(store.get("s1").map(|r| r.id), Some("s1".to_string()));
    assert_eq!(store.list().len(), 1);
    nc_code_services_conversation_share::handoff::assert_domain_is_send_sync();
}

#[tokio::test]
async fn wire_round_trip_through_nc_code_rpc() {
    let record =
        nc_code_services_conversation_share::handoff::ShareRecord::new("wire-1", "wire-kind");
    let envelope = nc_code_services_conversation_share::handoff::HandoffEnvelope::new(vec![record]);
    let text = serde_json::to_string(&envelope).expect("json");
    let wire = nc_code_rpc::WireValue::Json(
        serde_json::from_str::<serde_json::Value>(&text).expect("value"),
    );
    let mut buf = Vec::new();
    nc_code_rpc::encode_value(&wire, &mut buf);
    let mut cursor = 0;
    let decoded = nc_code_rpc::decode_value(&buf, &mut cursor).expect("decode");
    assert_eq!(decoded, wire);
}
