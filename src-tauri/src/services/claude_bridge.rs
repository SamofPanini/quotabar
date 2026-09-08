//! Synthetic, test-only contract spike for a future Claude bridge.
//!
//! This module deliberately has no credential, network, IPC, or persistence integration.
//! Its JSON inputs are mock transport payloads and are validated before normalization.

use chrono::DateTime;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteKind {
    ClaudeDesktop,
    ClaudeWeb,
    Mock,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct InstanceId(String);

impl InstanceId {
    fn new(value: &str) -> Result<Self, BridgeError> {
        valid_app_owned_label(value)
            .then(|| Self(value.to_owned()))
            .ok_or(SAFE_MALFORMED)
    }
}

impl fmt::Debug for InstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InstanceId(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq)]
struct RouteLabel(String);

impl RouteLabel {
    fn new(value: &str) -> Result<Self, BridgeError> {
        valid_app_owned_label(value)
            .then(|| Self(value.to_owned()))
            .ok_or(SAFE_MALFORMED)
    }
}

impl fmt::Debug for RouteLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RouteLabel(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Route {
    kind: RouteKind,
    label: Option<RouteLabel>,
}

impl Route {
    fn mock(label: Option<&str>) -> Result<Self, BridgeError> {
        Self::new(RouteKind::Mock, label)
    }

    fn claude_desktop(label: Option<&str>) -> Result<Self, BridgeError> {
        Self::new(RouteKind::ClaudeDesktop, label)
    }

    fn claude_web(label: Option<&str>) -> Result<Self, BridgeError> {
        Self::new(RouteKind::ClaudeWeb, label)
    }

    fn new(kind: RouteKind, label: Option<&str>) -> Result<Self, BridgeError> {
        Ok(Self {
            kind,
            label: label.map(RouteLabel::new).transpose()?,
        })
    }
}

impl fmt::Debug for Route {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Route")
            .field("kind", &self.kind)
            .field("label", &self.label.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsageWindowName {
    FiveHour,
    SevenDay,
    ExtraUsage,
}

impl UsageWindowName {
    fn protocol_name(self) -> &'static str {
        match self {
            Self::FiveHour => "five_hour",
            Self::SevenDay => "seven_day",
            Self::ExtraUsage => "extra_usage",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "five_hour" => Some(Self::FiveHour),
            "seven_day" => Some(Self::SevenDay),
            "extra_usage" => Some(Self::ExtraUsage),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct UsageWindow {
    name: UsageWindowName,
    used_percent: Option<f64>,
    reset_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    InternalUsageApi,
    CompletionSse,
    LocalEstimate,
    StaleCache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, PartialEq)]
struct Envelope {
    version: &'static str,
    instance_id: InstanceId,
    route: Route,
    snapshot: Option<Snapshot>,
    error: Option<BridgeError>,
}

impl fmt::Debug for Envelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Envelope")
            .field("version", &self.version)
            .field("instance_id", &"<redacted>")
            .field("route", &self.route)
            .field("snapshot", &self.snapshot)
            .field("error", &self.error)
            .finish()
    }
}

/// The only transport seam in P3. It is populated exclusively by unit tests.
#[derive(Default)]
struct MockTransport {
    responses: HashMap<InstanceId, MockResponse>,
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
    snapshots: HashMap<InstanceId, Envelope>,
}

impl SnapshotStore {
    fn insert(&mut self, envelope: Envelope) {
        self.snapshots.insert(envelope.instance_id.clone(), envelope);
    }

    fn get(&self, instance_id: &InstanceId) -> Option<&Envelope> {
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
    raw_instance_id: &str,
    route: Route,
    observed_at: &str,
) -> Result<Envelope, BridgeError> {
    let instance_id = InstanceId::new(raw_instance_id)?;
    if DateTime::parse_from_rfc3339(observed_at).is_err() {
        return Err(SAFE_MALFORMED);
    }
    if route.kind != RouteKind::Mock {
        return Err(BridgeError {
            code: ErrorCode::UnsupportedRoute,
            retryable: false,
        });
    }
    let response = transport.responses.get(&instance_id);
    let endpoint = response.and_then(|item| item.endpoint.as_ref());

    let result = match endpoint.map(|payload| parse_payload(payload, PayloadKind::Endpoint)) {
        Some(ParsedPayload::Rejected(error)) => unavailable(instance_id, route, error),
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

fn select_fallback(
    response: Option<&MockResponse>,
    instance_id: InstanceId,
    route: Route,
    observed_at: &str,
) -> Envelope {
    for (kind, payload) in [
        (
            PayloadKind::CompletionSse,
            response.and_then(|item| item.completion_sse.as_ref()),
        ),
        (
            PayloadKind::LocalEstimate,
            response.and_then(|item| item.local_estimate.as_ref()),
        ),
        (
            PayloadKind::StaleCache,
            response.and_then(|item| item.stale_cache.as_ref()),
        ),
    ] {
        let Some(payload) = payload else {
            continue;
        };
        match parse_payload(payload, kind) {
            ParsedPayload::Rejected(error) => {
                return unavailable(instance_id, route, error)
            }
            ParsedPayload::Windows(windows) if !windows.is_empty() => {
                return candidate(instance_id, route, observed_at, windows, kind)
            }
            ParsedPayload::Empty | ParsedPayload::Windows(_) => {
                return unavailable(instance_id, route, SAFE_MALFORMED)
            }
        }
    }
    unavailable(instance_id, route, SAFE_UNAVAILABLE)
}

fn candidate(
    instance_id: InstanceId,
    route: Route,
    observed_at: &str,
    windows: Vec<UsageWindow>,
    kind: PayloadKind,
) -> Envelope {
    let (source, confidence, error) = match kind {
        PayloadKind::CompletionSse => (Source::CompletionSse, Confidence::Observed, None),
        PayloadKind::LocalEstimate => (Source::LocalEstimate, Confidence::Estimated, None),
        PayloadKind::StaleCache => (
            Source::StaleCache,
            Confidence::Unknown,
            Some(BridgeError {
                code: ErrorCode::Stale,
                retryable: true,
            }),
        ),
        _ => unreachable!("endpoint candidates are handled before fallback selection"),
    };
    envelope(instance_id, route, observed_at, source, confidence, Some(windows), error)
}

fn envelope(
    instance_id: InstanceId,
    route: Route,
    observed_at: &str,
    source: Source,
    confidence: Confidence,
    usage_windows: Option<Vec<UsageWindow>>,
    error: Option<BridgeError>,
) -> Envelope {
    Envelope {
        version: VERSION,
        instance_id,
        route,
        snapshot: Some(Snapshot {
            source,
            confidence,
            observed_at: observed_at.to_owned(),
            usage_windows,
        }),
        error,
    }
}

fn unavailable(instance_id: InstanceId, route: Route, error: BridgeError) -> Envelope {
    Envelope {
        version: VERSION,
        instance_id,
        route,
        snapshot: None,
        error: Some(error),
    }
}

fn parse_payload(value: &Value, kind: PayloadKind) -> ParsedPayload {
    if contains_forbidden(value) {
        return ParsedPayload::Rejected(SAFE_REDACTED);
    }
    let Some(object) = value.as_object() else {
        return ParsedPayload::Rejected(SAFE_MALFORMED);
    };
    let key = match kind {
        PayloadKind::CompletionSse => "usageWindow",
        PayloadKind::Endpoint | PayloadKind::LocalEstimate | PayloadKind::StaleCache => {
            "usageWindows"
        }
    };
    if object.len() != 1 || !object.contains_key(key) {
        return ParsedPayload::Rejected(SAFE_MALFORMED);
    }
    let windows = match kind {
        PayloadKind::CompletionSse => object
            .get(key)
            .and_then(parse_window)
            .map(|window| vec![window]),
        PayloadKind::Endpoint | PayloadKind::LocalEstimate | PayloadKind::StaleCache => object
            .get(key)
            .and_then(Value::as_array)
            .and_then(|items| items.iter().map(parse_window).collect()),
    };
    match windows {
        Some(windows) if windows.is_empty() && matches!(kind, PayloadKind::Endpoint) => {
            ParsedPayload::Empty
        }
        Some(windows) if !windows.is_empty() => ParsedPayload::Windows(windows),
        _ => ParsedPayload::Rejected(SAFE_MALFORMED),
    }
}

fn parse_window(value: &Value) -> Option<UsageWindow> {
    let object = value.as_object()?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "name" | "usedPercent" | "resetAt"))
    {
        return None;
    }
    let name = UsageWindowName::parse(object.get("name")?.as_str()?)?;
    let used_percent = match object.get("usedPercent") {
        Some(value) => Some(value.as_f64()?),
        None => None,
    };
    if used_percent.is_some_and(|percent| !(0.0..=100.0).contains(&percent)) {
        return None;
    }
    let reset_at = match object.get("resetAt") {
        Some(value) => Some(value.as_str()?.to_owned()),
        None => None,
    };
    if reset_at
        .as_deref()
        .is_some_and(|timestamp| DateTime::parse_from_rfc3339(timestamp).is_err())
    {
        return None;
    }
    Some(UsageWindow {
        name,
        used_percent,
        reset_at,
    })
}

fn contains_forbidden(value: &Value) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| has_forbidden_fragment(key) || contains_forbidden(value)),
        Value::Array(items) => items.iter().any(contains_forbidden),
        Value::String(text) => has_forbidden_fragment(text),
        _ => false,
    }
}

/// `org` is rejected only as a complete `-`/`_`-delimited component, so a
/// harmless synthetic label such as `forged-demo` remains available. Every
/// other forbidden fragment is rejected anywhere in raw fixture text or labels.
fn has_forbidden_fragment(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("do_not_emit_sentinel")
        || [
            "organization",
            "email",
            "account",
            "token",
            "cookie",
            "session",
            "authorization",
            "provider",
            "raw",
        ]
        .iter()
        .any(|fragment| lower.contains(fragment))
        || lower
            .split(['-', '_'])
            .any(|component| component == "org")
}

fn validate_envelope(envelope: &Envelope) -> Result<(), BridgeError> {
    if envelope.version != VERSION || envelope.route.kind != RouteKind::Mock {
        return Err(SAFE_MALFORMED);
    }
    let Some(snapshot) = &envelope.snapshot else {
        return match envelope.error {
            Some(error) if error.code != ErrorCode::Stale => Ok(()),
            _ => Err(SAFE_MALFORMED),
        };
    };
    if DateTime::parse_from_rfc3339(&snapshot.observed_at).is_err() {
        return Err(SAFE_MALFORMED);
    }
    let source_confidence_valid = matches!(
        (snapshot.source, snapshot.confidence),
        (Source::InternalUsageApi, Confidence::VerifiedServer)
            | (Source::CompletionSse, Confidence::Observed)
            | (Source::LocalEstimate, Confidence::Estimated)
            | (Source::StaleCache, Confidence::Unknown)
    );
    let windows_valid = snapshot.usage_windows.as_ref().is_some_and(|windows| {
        !windows.is_empty()
            && windows.iter().all(|window| {
                !window
                    .used_percent
                    .is_some_and(|percent| !(0.0..=100.0).contains(&percent))
                    && !window.reset_at.as_deref().is_some_and(|timestamp| {
                        DateTime::parse_from_rfc3339(timestamp).is_err()
                    })
            })
    });
    let error_valid = match (snapshot.source, envelope.error) {
        (Source::StaleCache, Some(BridgeError { code: ErrorCode::Stale, .. })) => true,
        (Source::StaleCache, _) => false,
        (_, None) => true,
        (_, Some(_)) => false,
    };
    if !source_confidence_valid || !windows_valid || !error_valid {
        return Err(SAFE_MALFORMED);
    }
    Ok(())
}

fn valid_app_owned_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.contains('@')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && !has_forbidden_fragment(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TIME: &str = "2026-01-01T00:00:00Z";

    fn instance(value: &str) -> InstanceId {
        InstanceId::new(value).unwrap()
    }

    fn route(label: &str) -> Route {
        Route::mock(Some(label)).unwrap()
    }

    fn window(name: UsageWindowName, percent: f64) -> Value {
        json!({
            "name": name.protocol_name(),
            "usedPercent": percent,
            "resetAt": "2026-01-02T00:00:00Z",
        })
    }

    fn response(
        endpoint: Option<Value>,
        completion_sse: Option<Value>,
        local_estimate: Option<Value>,
        stale_cache: Option<Value>,
    ) -> MockResponse {
        MockResponse {
            endpoint,
            completion_sse,
            local_estimate,
            stale_cache,
        }
    }

    #[test]
    fn paid_endpoint_is_verified_and_stored_by_opaque_instance() {
        let mut transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        transport.responses.insert(
            instance("paid-instance"),
            response(
                Some(json!({"usageWindows": [window(UsageWindowName::FiveHour, 42.0)]})),
                None,
                None,
                None,
            ),
        );

        let output = normalize(
            &transport,
            &mut store,
            "paid-instance",
            route("paid-demo"),
            TIME,
        )
        .unwrap();
        assert_eq!(
            output.snapshot.as_ref().unwrap().source,
            Source::InternalUsageApi
        );
        assert_eq!(
            output.snapshot.as_ref().unwrap().confidence,
            Confidence::VerifiedServer
        );
        assert_eq!(store.get(&instance("paid-instance")), Some(&output));
    }

    #[test]
    fn empty_endpoint_allows_sse_but_never_invents_zero_usage() {
        let mut transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        transport.responses.insert(
            instance("free-instance"),
            response(
                Some(json!({"usageWindows": []})),
                Some(json!({"usageWindow": window(UsageWindowName::SevenDay, 13.0)})),
                None,
                None,
            ),
        );

        let sse = normalize(
            &transport,
            &mut store,
            "free-instance",
            route("free-demo"),
            TIME,
        )
        .unwrap();
        assert_eq!(sse.snapshot.as_ref().unwrap().source, Source::CompletionSse);
        assert_eq!(
            sse.snapshot.as_ref().unwrap().usage_windows.as_ref().unwrap()[0].name,
            UsageWindowName::SevenDay
        );

        transport.responses.insert(
            instance("empty-instance"),
            response(Some(json!({"usageWindows": []})), None, None, None),
        );
        let empty = normalize(
            &transport,
            &mut store,
            "empty-instance",
            route("empty-demo"),
            TIME,
        )
        .unwrap();
        assert_eq!(empty.snapshot, None);
        assert_eq!(empty.error, Some(SAFE_UNAVAILABLE));

        let missing = normalize(
            &transport,
            &mut store,
            "missing-instance",
            route("missing-demo"),
            TIME,
        )
        .unwrap();
        assert_eq!(missing.snapshot, None);
        assert_eq!(missing.error, Some(SAFE_UNAVAILABLE));
    }

    #[test]
    fn endpoint_precedes_estimate_and_stale_cache() {
        let mut transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        transport.responses.insert(
            instance("priority-instance"),
            response(
                Some(json!({"usageWindows": [window(UsageWindowName::FiveHour, 80.0)]})),
                Some(json!({"usageWindow": window(UsageWindowName::SevenDay, 20.0)})),
                Some(json!({"usageWindows": [window(UsageWindowName::ExtraUsage, 10.0)]})),
                Some(json!({"usageWindows": [window(UsageWindowName::FiveHour, 5.0)]})),
            ),
        );
        assert_eq!(
            normalize(
                &transport,
                &mut store,
                "priority-instance",
                route("priority-demo"),
                TIME,
            )
            .unwrap()
            .snapshot
            .unwrap()
            .source,
            Source::InternalUsageApi
        );
    }

    #[test]
    fn local_estimate_precedes_stale_and_real_stale_is_marked_stale() {
        let mut transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        transport.responses.insert(
            instance("estimate-instance"),
            response(
                None,
                None,
                Some(json!({"usageWindows": [window(UsageWindowName::SevenDay, 22.0)]})),
                Some(json!({"usageWindows": [window(UsageWindowName::ExtraUsage, 7.0)]})),
            ),
        );
        let estimate = normalize(
            &transport,
            &mut store,
            "estimate-instance",
            route("estimate-demo"),
            TIME,
        )
        .unwrap();
        assert_eq!(estimate.snapshot.as_ref().unwrap().source, Source::LocalEstimate);

        transport.responses.insert(
            instance("stale-instance"),
            response(
                None,
                None,
                None,
                Some(json!({"usageWindows": [window(UsageWindowName::ExtraUsage, 7.0)]})),
            ),
        );
        let stale = normalize(
            &transport,
            &mut store,
            "stale-instance",
            route("stale-demo"),
            TIME,
        )
        .unwrap();
        assert_eq!(stale.snapshot.as_ref().unwrap().source, Source::StaleCache);
        assert_eq!(stale.error.map(|error| error.code), Some(ErrorCode::Stale));
    }

    #[test]
    fn malformed_sources_and_unknown_window_names_fail_closed() {
        let invalid_names = ["providerish", "email", "session", "token", "not-a-window"];
        for name in invalid_names {
            let raw = json!({"usageWindows": [{"name": name, "usedPercent": 1.0}]});
            assert!(matches!(
                parse_payload(&raw, PayloadKind::Endpoint),
                ParsedPayload::Rejected(_)
            ));
        }
        let unknown = json!({
            "usageWindows": [window(UsageWindowName::FiveHour, 1.0)],
            "unexpected": true,
        });
        assert!(matches!(
            parse_payload(&unknown, PayloadKind::Endpoint),
            ParsedPayload::Rejected(error) if error == SAFE_MALFORMED
        ));
        let cases = [
            ("bad-sse", response(None, Some(json!({"usageWindow": []})), None, None)),
            (
                "bad-estimate",
                response(None, None, Some(json!({"usageWindows": []})), None),
            ),
            (
                "bad-stale",
                response(None, None, None, Some(json!({"usageWindows": []}))),
            ),
        ];
        let mut transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        for (id, payload) in cases {
            transport.responses.insert(instance(id), payload);
            let result =
                normalize(&transport, &mut store, id, route("bad-demo"), TIME).unwrap();
            assert_eq!(result.snapshot, None);
            assert_eq!(result.error, Some(SAFE_MALFORMED));
        }
    }

    #[test]
    fn forbidden_keys_values_and_sentinel_are_redacted() {
        let forbidden = json!({
            "usageWindows": [window(UsageWindowName::FiveHour, 1.0)],
            "authorization": "DO_NOT_EMIT_SENTINEL",
        });
        assert!(matches!(
            parse_payload(&forbidden, PayloadKind::Endpoint),
            ParsedPayload::Rejected(error) if error == SAFE_REDACTED
        ));
        let mut transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        transport.responses.insert(
            instance("safe-instance"),
            response(Some(forbidden), None, None, None),
        );
        let output = normalize(
            &transport,
            &mut store,
            "safe-instance",
            route("safe-route"),
            TIME,
        )
        .unwrap();
        assert_eq!(output.error, Some(SAFE_REDACTED));
        let debug = format!("{output:?}");
        for hidden in ["safe-instance", "safe-route", "authorization", "DO_NOT_EMIT_SENTINEL"] {
            assert!(!debug.contains(hidden));
        }
    }

    #[test]
    fn opaque_constructors_reject_providerish_sentinel_and_identity_fragments() {
        for unsafe_label in [
            "providerish",
            "demo-org-label",
            "email-demo",
            "session-demo",
            "token-demo",
            "DO_NOT_EMIT_SENTINEL",
            "route@label",
            "a label with spaces",
        ] {
            assert_eq!(InstanceId::new(unsafe_label), Err(SAFE_MALFORMED));
            assert_eq!(RouteLabel::new(unsafe_label), Err(SAFE_MALFORMED));
        }
        assert!(InstanceId::new("forged-demo").is_ok());
        assert!(RouteLabel::new("forged-demo").is_ok());
    }

    #[test]
    fn invalid_inputs_and_real_routes_do_not_write_the_store() {
        let transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        assert_eq!(
            normalize(
                &transport,
                &mut store,
                "invalid instance",
                route("valid-demo"),
                TIME,
            ),
            Err(SAFE_MALFORMED)
        );
        assert_eq!(Route::mock(Some("invalid label")), Err(SAFE_MALFORMED));
        let unsupported = Err(BridgeError {
            code: ErrorCode::UnsupportedRoute,
            retryable: false,
        });
        assert_eq!(
            normalize(
                &transport,
                &mut store,
                "desktop-instance",
                Route::claude_desktop(Some("desktop-demo")).unwrap(),
                TIME,
            ),
            unsupported
        );
        assert_eq!(
            normalize(
                &transport,
                &mut store,
                "web-instance",
                Route::claude_web(Some("web-demo")).unwrap(),
                TIME,
            ),
            unsupported
        );
        assert!(store.snapshots.is_empty());
    }

    #[test]
    fn failed_update_replaces_only_that_instances_last_good_snapshot() {
        let mut transport = MockTransport::default();
        let mut store = SnapshotStore::default();
        for (id, name) in [
            ("first-instance", UsageWindowName::FiveHour),
            ("second-instance", UsageWindowName::SevenDay),
        ] {
            transport.responses.insert(
                instance(id),
                response(
                    Some(json!({"usageWindows": [window(name, 40.0)]})),
                    None,
                    None,
                    None,
                ),
            );
            normalize(&transport, &mut store, id, route("safe-route"), TIME).unwrap();
        }
        transport.responses.insert(
            instance("first-instance"),
            response(Some(json!({"usageWindows": [], "extra": true})), None, None, None),
        );
        let failed = normalize(
            &transport,
            &mut store,
            "first-instance",
            route("safe-route"),
            TIME,
        )
        .unwrap();
        assert_eq!(failed.snapshot, None);
        assert_eq!(store.get(&instance("first-instance")), Some(&failed));
        assert_eq!(
            store
                .get(&instance("second-instance"))
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .usage_windows
                .as_ref()
                .unwrap()[0]
                .name,
            UsageWindowName::SevenDay
        );
    }

    #[test]
    fn stale_invariant_rejects_fabricated_stale_snapshots() {
        let route = route("demo");
        let no_snapshot_stale = unavailable(
            instance("demo-instance"),
            route.clone(),
            BridgeError {
                code: ErrorCode::Stale,
                retryable: true,
            },
        );
        assert_eq!(validate_envelope(&no_snapshot_stale), Err(SAFE_MALFORMED));
        let stale_without_error = envelope(
            instance("demo-instance"),
            route,
            TIME,
            Source::StaleCache,
            Confidence::Unknown,
            Some(vec![UsageWindow {
                name: UsageWindowName::ExtraUsage,
                used_percent: None,
                reset_at: None,
            }]),
            None,
        );
        assert_eq!(validate_envelope(&stale_without_error), Err(SAFE_MALFORMED));
    }
}
