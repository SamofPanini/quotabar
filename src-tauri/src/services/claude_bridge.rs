//! Synthetic, test-only contract spike for a future Claude bridge.
//!
//! This module deliberately has no credential, network, IPC, or persistence integration.
//! Its JSON inputs are mock transport payloads and are validated before normalization.

use chrono::DateTime;
use serde_json::Value;
use std::collections::HashMap;

const VERSION: &str = "v1";
const SAFE_UNAVAILABLE: BridgeError = BridgeError {
    code: ErrorCode::Unavailable,
    retryable: true,
};
const SAFE_MALFORMED: BridgeError = BridgeError {
    code: ErrorCode::MalformedPayload,
    retryable: false,
};
const SAFE_REDACTED: BridgeError = BridgeError {
    code: ErrorCode::RedactedField,
    retryable: false,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum RouteKind {
    ClaudeDesktop,
    ClaudeWeb,
    Mock,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Route {
    kind: RouteKind,
    label: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct UsageWindow {
    name: String,
    used_percent: Option<f64>,
    reset_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    InternalUsageApi,
    CompletionSse,
    LocalEstimate,
    StaleCache,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Confidence {
    VerifiedServer,
    Observed,
    Estimated,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    source: Source,
    confidence: Confidence,
    observed_at: String,
    usage_windows: Option<Vec<UsageWindow>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorCode {
    Unavailable,
    MalformedPayload,
    UnsupportedRoute,
    RedactedField,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BridgeError {
    code: ErrorCode,
    retryable: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Envelope {
    version: &'static str,
    instance_id: String,
    route: Route,
    snapshot: Option<Snapshot>,
    error: Option<BridgeError>,
}

/// The only transport seam in P3. It is populated exclusively by unit tests.
#[derive(Default)]
struct MockTransport {
    responses: HashMap<String, MockResponse>,
}

#[derive(Default)]
struct MockResponse {
    endpoint: Option<Value>,
    completion_sse: Option<Value>,
    local_estimate: Option<Value>,
    stale_cache: Option<Value>,
}

#[derive(Default)]
struct SnapshotStore {
    snapshots: HashMap<String, Envelope>,
}

impl SnapshotStore {
    fn insert(&mut self, envelope: Envelope) {
        self.snapshots.insert(envelope.instance_id.clone(), envelope);
    }

    fn get(&self, instance_id: &str) -> Option<&Envelope> {
        self.snapshots.get(instance_id)
    }
}

#[derive(Clone, Copy)]
enum PayloadKind {
    Endpoint,
    CompletionSse,
    LocalEstimate,
    StaleCache,
}

enum ParsedPayload {
    Windows(Vec<UsageWindow>),
    Empty,
    Rejected(BridgeError),
}

fn normalize(
    transport: &MockTransport,
    store: &mut SnapshotStore,
    instance_id: &str,
    route: Route,
    observed_at: &str,
) -> Result<Envelope, BridgeError> {
    if !valid_app_id(instance_id)
        || !valid_route(&route)
        || DateTime::parse_from_rfc3339(observed_at).is_err()
    {
        return Err(SAFE_MALFORMED);
    }
    let response = transport.responses.get(instance_id);
    let endpoint = response.and_then(|item| item.endpoint.as_ref());

    let result = match endpoint.map(|payload| parse_payload(payload, PayloadKind::Endpoint)) {
        Some(ParsedPayload::Rejected(error)) => unavailable(instance_id, route, observed_at, error),
        Some(ParsedPayload::Windows(windows)) if !windows.is_empty() => envelope(
            instance_id,
            route,
            observed_at,
            Source::InternalUsageApi,
            Confidence::VerifiedServer,
            Some(windows),
            None,
        ),
        Some(ParsedPayload::Empty) | Some(ParsedPayload::Windows(_)) | None => {
            select_fallback(response, instance_id, route, observed_at)
        }
    };

    validate_envelope(&result)?;
    store.insert(result.clone());
    Ok(result)
}

fn select_fallback(response: Option<&MockResponse>, instance_id: &str, route: Route, observed_at: &str) -> Envelope {
    for (kind, payload) in [
        (PayloadKind::CompletionSse, response.and_then(|item| item.completion_sse.as_ref())),
        (PayloadKind::LocalEstimate, response.and_then(|item| item.local_estimate.as_ref())),
        (PayloadKind::StaleCache, response.and_then(|item| item.stale_cache.as_ref())),
    ] {
        let Some(payload) = payload else { continue; };
        match parse_payload(payload, kind) {
            ParsedPayload::Rejected(error) => return unavailable(instance_id, route, observed_at, error),
            ParsedPayload::Windows(windows) if !windows.is_empty() => return candidate(instance_id, route, observed_at, windows, kind),
            ParsedPayload::Empty | ParsedPayload::Windows(_) => return unavailable(instance_id, route, observed_at, SAFE_MALFORMED),
        }
    }
    unavailable(instance_id, route, observed_at, SAFE_UNAVAILABLE)
}

fn candidate(instance_id: &str, route: Route, observed_at: &str, windows: Vec<UsageWindow>, kind: PayloadKind) -> Envelope {
    let (source, confidence, error) = match kind {
        PayloadKind::CompletionSse => (Source::CompletionSse, Confidence::Observed, None),
        PayloadKind::LocalEstimate => (Source::LocalEstimate, Confidence::Estimated, None),
        PayloadKind::StaleCache => (Source::StaleCache, Confidence::Unknown, Some(BridgeError { code: ErrorCode::Stale, retryable: true })),
        _ => unreachable!("endpoint candidates are handled before fallback selection"),
    };
    envelope(instance_id, route, observed_at, source, confidence, Some(windows), error)
}

fn envelope(instance_id: &str, route: Route, observed_at: &str, source: Source, confidence: Confidence, usage_windows: Option<Vec<UsageWindow>>, error: Option<BridgeError>) -> Envelope {
    Envelope { version: VERSION, instance_id: instance_id.to_owned(), route, snapshot: Some(Snapshot { source, confidence, observed_at: observed_at.to_owned(), usage_windows }), error }
}

fn unavailable(instance_id: &str, route: Route, _observed_at: &str, error: BridgeError) -> Envelope {
    Envelope {
        version: VERSION,
        instance_id: instance_id.to_owned(),
        route,
        snapshot: None,
        error: Some(error),
    }
}

fn parse_payload(value: &Value, kind: PayloadKind) -> ParsedPayload {
    if contains_forbidden(value) { return ParsedPayload::Rejected(SAFE_REDACTED); }
    let Some(object) = value.as_object() else { return ParsedPayload::Rejected(SAFE_MALFORMED); };
    let key = match kind { PayloadKind::CompletionSse => "usageWindow", _ => "usageWindows" };
    if object.len() != 1 || !object.contains_key(key) { return ParsedPayload::Rejected(SAFE_MALFORMED); }
    let windows = match kind {
        PayloadKind::CompletionSse => object.get(key).and_then(parse_window).map(|window| vec![window]),
        _ => object.get(key).and_then(Value::as_array).and_then(|items| items.iter().map(parse_window).collect()),
    };
    match windows { Some(windows) if windows.is_empty() && matches!(kind, PayloadKind::Endpoint) => ParsedPayload::Empty, Some(windows) if !windows.is_empty() => ParsedPayload::Windows(windows), _ => ParsedPayload::Rejected(SAFE_MALFORMED) }
}

fn parse_window(value: &Value) -> Option<UsageWindow> {
    let object = value.as_object()?;
    if object.keys().any(|key| !matches!(key.as_str(), "name" | "usedPercent" | "resetAt")) { return None; }
    let name = object.get("name")?.as_str()?.to_owned();
    let used_percent = match object.get("usedPercent") {
        Some(value) => Some(value.as_f64()?),
        None => None,
    };
    if used_percent.is_some_and(|percent| !(0.0..=100.0).contains(&percent)) { return None; }
    let reset_at = match object.get("resetAt") {
        Some(value) => Some(value.as_str()?.to_owned()),
        None => None,
    };
    if reset_at.as_deref().is_some_and(|timestamp| DateTime::parse_from_rfc3339(timestamp).is_err()) { return None; }
    Some(UsageWindow { name, used_percent, reset_at })
}

fn contains_forbidden(value: &Value) -> bool {
    const FORBIDDEN: &[&str] = &["organization", "org", "email", "account", "token", "cookie", "session", "authorization", "provider", "raw"];
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| FORBIDDEN.iter().any(|word| key.to_ascii_lowercase().contains(word)) || contains_forbidden(value)),
        Value::Array(items) => items.iter().any(contains_forbidden),
        Value::String(text) => text == "DO_NOT_EMIT_SENTINEL",
        _ => false,
    }
}

fn validate_envelope(envelope: &Envelope) -> Result<(), BridgeError> {
    if envelope.version != VERSION || !valid_app_id(&envelope.instance_id) || !valid_route(&envelope.route) { return Err(SAFE_MALFORMED); }
    let Some(snapshot) = &envelope.snapshot else {
        return match envelope.error {
            Some(error) if error.code != ErrorCode::Stale => Ok(()),
            _ => Err(SAFE_MALFORMED),
        };
    };
    if DateTime::parse_from_rfc3339(&snapshot.observed_at).is_err() { return Err(SAFE_MALFORMED); }
    let source_confidence_valid = matches!(
        (&snapshot.source, &snapshot.confidence),
        (Source::InternalUsageApi, Confidence::VerifiedServer)
            | (Source::CompletionSse, Confidence::Observed)
            | (Source::LocalEstimate, Confidence::Estimated)
            | (Source::StaleCache, Confidence::Unknown)
    );
    let windows_valid = snapshot.usage_windows.as_ref().is_none_or(|windows| {
        !windows.is_empty()
            && windows.iter().all(|window| {
                !window.name.is_empty()
                    && !window.used_percent.is_some_and(|percent| !(0.0..=100.0).contains(&percent))
                    && !window.reset_at.as_deref().is_some_and(|timestamp| DateTime::parse_from_rfc3339(timestamp).is_err())
            })
    });
    let error_valid = match (&snapshot.source, envelope.error) {
        (Source::StaleCache, Some(BridgeError { code: ErrorCode::Stale, .. })) => true,
        (Source::StaleCache, _) => false,
        (_, None) => true,
        (_, Some(_)) => false,
    };
    if !source_confidence_valid || !windows_valid || !error_valid { return Err(SAFE_MALFORMED); }
    Ok(())
}

fn valid_app_id(value: &str) -> bool { !value.is_empty() && value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) }
fn valid_route(route: &Route) -> bool { route.label.as_deref().is_none_or(valid_app_id) }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TIME: &str = "2026-01-01T00:00:00Z";
    fn route(label: &str) -> Route { Route { kind: RouteKind::Mock, label: Some(label.into()) } }
    fn window(name: &str, percent: f64) -> Value { json!({ "name": name, "usedPercent": percent, "resetAt": "2026-01-02T00:00:00Z" }) }
    fn response(endpoint: Option<Value>, sse: Option<Value>, estimate: Option<Value>, stale: Option<Value>) -> MockResponse { MockResponse { endpoint, completion_sse: sse, local_estimate: estimate, stale_cache: stale } }

    #[test]
    fn paid_like_endpoint_is_verified_and_stored_by_instance() {
        let mut transport = MockTransport::default(); let mut store = SnapshotStore::default();
        transport.responses.insert("paid-instance".into(), response(Some(json!({"usageWindows": [window("five-hour", 42.0)]})), None, None, None));
        let result = normalize(&transport, &mut store, "paid-instance", route("paid-demo"), TIME).unwrap();
        assert_eq!(result.snapshot.as_ref().unwrap().source, Source::InternalUsageApi); assert_eq!(result.snapshot.as_ref().unwrap().confidence, Confidence::VerifiedServer); assert_eq!(store.get("paid-instance"), Some(&result));
    }

    #[test]
    fn free_empty_endpoint_uses_sse_without_inventing_endpoint_usage() {
        let mut transport = MockTransport::default(); let mut store = SnapshotStore::default();
        transport.responses.insert("free-instance".into(), response(Some(json!({"usageWindows": []})), Some(json!({"usageWindow": window("completion", 13.0)})), None, None));
        let result = normalize(&transport, &mut store, "free-instance", route("free-demo"), TIME).unwrap();
        assert_eq!(result.snapshot.as_ref().unwrap().source, Source::CompletionSse); assert_eq!(result.snapshot.as_ref().unwrap().confidence, Confidence::Observed); assert_eq!(result.snapshot.unwrap().usage_windows.unwrap()[0].name, "completion");
    }

    #[test]
    fn empty_endpoint_alone_is_unavailable_not_zero() {
        let mut transport = MockTransport::default(); let mut store = SnapshotStore::default();
        transport.responses.insert("free-instance".into(), response(Some(json!({"usageWindows": []})), None, None, None));
        let result = normalize(&transport, &mut store, "free-instance", route("free-demo"), TIME).unwrap();
        assert_eq!(result.error, Some(SAFE_UNAVAILABLE)); assert_eq!(result.snapshot, None);
    }

    #[test]
    fn missing_response_is_unavailable_without_a_fabricated_snapshot() {
        let transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        let result = normalize(&transport, &mut store, "missing-instance", route("missing-demo"), TIME).unwrap();
        assert_eq!(result.error, Some(SAFE_UNAVAILABLE));
        assert_eq!(result.snapshot, None);
    }

    #[test]
    fn invalid_instance_route_or_time_never_writes_the_store() {
        let transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        assert_eq!(normalize(&transport, &mut store, "invalid instance", route("valid-demo"), TIME), Err(SAFE_MALFORMED));
        assert_eq!(normalize(&transport, &mut store, "route-instance", route("invalid label"), TIME), Err(SAFE_MALFORMED));
        assert_eq!(normalize(&transport, &mut store, "time-instance", route("time-demo"), "not-a-timestamp"), Err(SAFE_MALFORMED));
        assert!(store.snapshots.is_empty());
    }

    #[test]
    fn endpoint_precedence_and_malformed_fail_closed() {
        let mut transport = MockTransport::default(); let mut store = SnapshotStore::default();
        transport.responses.insert("paid-instance".into(), response(Some(json!({"usageWindows": [window("endpoint", 80.0)]})), Some(json!({"usageWindow": window("sse", 20.0)})), Some(json!({"usageWindows": [window("estimate", 10.0)]})), Some(json!({"usageWindows": [window("cache", 5.0)]}))));
        assert_eq!(normalize(&transport, &mut store, "paid-instance", route("paid-demo"), TIME).unwrap().snapshot.unwrap().source, Source::InternalUsageApi);
        transport.responses.insert("bad-instance".into(), response(Some(json!({"usageWindows": [], "extra": true})), Some(json!({"usageWindow": window("sse", 20.0)})), None, None));
        let bad = normalize(&transport, &mut store, "bad-instance", route("bad-demo"), TIME).unwrap();
        assert_eq!(bad.error, Some(SAFE_MALFORMED)); assert_eq!(bad.snapshot, None);
    }

    #[test]
    fn stale_cache_and_instances_do_not_cross() {
        let mut transport = MockTransport::default(); let mut store = SnapshotStore::default();
        transport.responses.insert("paid-instance".into(), response(Some(json!({"usageWindows": [window("paid", 60.0)]})), None, None, None));
        transport.responses.insert("free-instance".into(), response(None, None, None, Some(json!({"usageWindows": [window("free-cache", 7.0)]}))));
        normalize(&transport, &mut store, "paid-instance", route("paid-demo"), TIME).unwrap();
        let free = normalize(&transport, &mut store, "free-instance", route("free-demo"), TIME).unwrap();
        assert_eq!(free.snapshot.as_ref().unwrap().source, Source::StaleCache); assert_eq!(free.error.map(|error| error.code), Some(ErrorCode::Stale));
        assert_eq!(store.get("paid-instance").unwrap().snapshot.as_ref().unwrap().usage_windows.as_ref().unwrap()[0].name, "paid");
        assert_eq!(store.get("free-instance").unwrap().snapshot.as_ref().unwrap().usage_windows.as_ref().unwrap()[0].name, "free-cache");
    }

    #[test]
    fn local_estimate_precedes_stale_cache_and_keeps_its_own_source() {
        let mut transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        transport.responses.insert("estimate-instance".into(), response(None, None, Some(json!({"usageWindows": [window("estimate", 22.0)]})), Some(json!({"usageWindows": [window("cache", 7.0)]}))));
        let result = normalize(&transport, &mut store, "estimate-instance", route("estimate-demo"), TIME).unwrap();
        assert_eq!(result.snapshot.as_ref().unwrap().source, Source::LocalEstimate);
        assert_eq!(result.snapshot.as_ref().unwrap().confidence, Confidence::Estimated);
        assert_eq!(result.error, None);
    }

    #[test]
    fn forbidden_unknown_and_sentinel_values_fail_closed_without_leaking() {
        let forbidden = json!({"usageWindows": [window("safe", 1.0)], "authorization": "DO_NOT_EMIT_SENTINEL"});
        let unknown = json!({"usageWindows": [window("safe", 1.0)], "unexpected": true});
        assert!(matches!(parse_payload(&forbidden, PayloadKind::Endpoint), ParsedPayload::Rejected(error) if error == SAFE_REDACTED));
        assert!(matches!(parse_payload(&unknown, PayloadKind::Endpoint), ParsedPayload::Rejected(error) if error == SAFE_MALFORMED));
        let mut transport = MockTransport::default(); let mut store = SnapshotStore::default();
        transport.responses.insert("safe-instance".into(), response(Some(forbidden), None, None, None));
        let output = normalize(&transport, &mut store, "safe-instance", route("safe-demo"), TIME).unwrap();
        assert_eq!(output.error, Some(SAFE_REDACTED));
        let output_debug = format!("{output:?}");
        assert!(!output_debug.contains("DO_NOT_EMIT_SENTINEL"));
        assert!(!output_debug.contains("authorization"));
        let safe = format!("{:?}", SAFE_REDACTED); assert!(!safe.contains("DO_NOT_EMIT_SENTINEL")); assert!(!safe.contains("authorization"));
    }

    #[test]
    fn envelope_validation_rejects_bad_version_source_confidence_and_window() {
        let mut valid = envelope("demo-instance", route("demo"), TIME, Source::InternalUsageApi, Confidence::VerifiedServer, Some(vec![UsageWindow { name: "window".into(), used_percent: Some(5.0), reset_at: None }]), None);
        assert!(validate_envelope(&valid).is_ok());
        valid.version = "v0"; assert_eq!(validate_envelope(&valid), Err(SAFE_MALFORMED));
        valid.version = VERSION;
        valid.snapshot.as_mut().unwrap().confidence = Confidence::Observed;
        assert_eq!(validate_envelope(&valid), Err(SAFE_MALFORMED));
        valid.snapshot.as_mut().unwrap().confidence = Confidence::VerifiedServer;
        valid.snapshot.as_mut().unwrap().usage_windows = Some(vec![UsageWindow { name: "".into(), used_percent: None, reset_at: None }]);
        assert_eq!(validate_envelope(&valid), Err(SAFE_MALFORMED));
    }

    #[test]
    fn envelope_state_invariants_reject_fabricated_or_mixed_errors() {
        let route = route("demo");
        let no_snapshot_stale = unavailable("demo-instance", route.clone(), TIME, BridgeError { code: ErrorCode::Stale, retryable: true });
        assert_eq!(validate_envelope(&no_snapshot_stale), Err(SAFE_MALFORMED));
        let stale_without_error = envelope("demo-instance", route.clone(), TIME, Source::StaleCache, Confidence::Unknown, Some(vec![UsageWindow { name: "cache".into(), used_percent: None, reset_at: None }]), None);
        assert_eq!(validate_envelope(&stale_without_error), Err(SAFE_MALFORMED));
        let verified_with_error = envelope("demo-instance", route, TIME, Source::InternalUsageApi, Confidence::VerifiedServer, Some(vec![UsageWindow { name: "endpoint".into(), used_percent: None, reset_at: None }]), Some(SAFE_UNAVAILABLE));
        assert_eq!(validate_envelope(&verified_with_error), Err(SAFE_MALFORMED));
    }
}
