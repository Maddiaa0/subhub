use super::attempts::Attempt;
use super::proto::transform_service_server::TransformService;
use super::proto::{
    HeaderValues, HttpRequest, HttpResponse, TransformAction, TransformRequestRequest,
    TransformRequestResponse, TransformResponseRequest, TransformResponseResponse,
};
use crate::gateway::routing::{apply_credential_headers, request_model, select_initial};
use crate::gateway::state::ProxyState;
use crate::provider::Provider;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use rand::Rng;
use std::collections::HashMap;
use tonic::{Request, Response, Status};

const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");

#[derive(Clone)]
pub(crate) struct IronTransform {
    state: ProxyState,
}

impl IronTransform {
    pub(crate) fn new(state: ProxyState) -> Self {
        Self { state }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Target {
    provider: Provider,
    host: String,
    method: String,
    path: String,
}

#[tonic::async_trait]
impl TransformService for IronTransform {
    async fn transform_request(
        &self,
        request: Request<TransformRequestRequest>,
    ) -> std::result::Result<Response<TransformRequestResponse>, Status> {
        let input = request.into_inner();
        let Some(mut original) = input.request else {
            return Ok(Response::new(reject(
                StatusCode::BAD_REQUEST,
                "Iron transform request did not contain an HTTP request",
            )));
        };
        let target = match Target::from_request(input.context.as_ref(), &original) {
            Ok(target) => target,
            Err(message) => {
                return Ok(Response::new(reject(StatusCode::FORBIDDEN, &message)));
            }
        };
        let model = request_model(&original.body);
        let credential = match select_initial(&self.state, target.provider, model.as_deref()).await
        {
            Ok(credential) => credential,
            Err(error) => {
                return Ok(Response::new(reject(
                    StatusCode::TOO_MANY_REQUESTS,
                    &error.to_string(),
                )));
            }
        };
        let mut headers = match proto_headers_to_http(&original.headers) {
            Ok(headers) => headers,
            Err(message) => {
                return Ok(Response::new(reject(StatusCode::BAD_REQUEST, &message)));
            }
        };
        if let Err(error) = apply_credential_headers(&mut headers, &credential) {
            return Ok(Response::new(reject(
                StatusCode::BAD_GATEWAY,
                &error.to_string(),
            )));
        }
        // Correlation controls access to the retry decision. Always replace
        // an untrusted client value so two sandbox requests cannot collide in
        // the attempt store by deliberately reusing a valid traceparent.
        let traceparent = new_traceparent();
        debug_assert!(valid_traceparent(&traceparent));
        headers.insert(
            TRACEPARENT,
            HeaderValue::from_str(&traceparent).expect("generated traceparent is valid"),
        );
        original.headers = http_headers_to_proto(&headers);
        self.state.iron_attempts.lock().await.insert(
            traceparent.clone(),
            Attempt::new(
                target.provider,
                credential.name.clone(),
                model.clone(),
                target.host,
                target.method,
                target.path,
            ),
        );
        crate::observability::event(
            "iron_request_authorized",
            serde_json::json!({
                "provider": target.provider,
                "credential": credential.name,
                "model": model
            }),
        );
        Ok(Response::new(TransformRequestResponse {
            action: TransformAction::Continue as i32,
            response: None,
            modified_request: Some(original),
            annotations: HashMap::from([
                ("provider".into(), target.provider.name().into()),
                ("selected_credential".into(), credential.name),
                ("traceparent".into(), traceparent),
            ]),
        }))
    }

    async fn transform_response(
        &self,
        request: Request<TransformResponseRequest>,
    ) -> std::result::Result<Response<TransformResponseResponse>, Status> {
        let input = request.into_inner();
        let traceparent = input
            .request
            .as_ref()
            .and_then(|request| proto_header(&request.headers, "traceparent"));
        if let Some(traceparent) = traceparent
            && let Some(attempt) = self
                .state
                .iron_attempts
                .lock()
                .await
                .remove_traceparent(&traceparent)
        {
            crate::observability::event(
                "iron_request_completed",
                serde_json::json!({
                    "provider": attempt.provider,
                    "credential": attempt.retry_credential_name
                        .as_deref()
                        .unwrap_or(&attempt.credential_name),
                    "status": input.response.as_ref().map(|response| response.status_code)
                }),
            );
        }
        Ok(Response::new(TransformResponseResponse {
            action: TransformAction::Continue as i32,
            modified_response: None,
            annotations: HashMap::new(),
        }))
    }
}

impl Target {
    fn from_request(
        context: Option<&super::proto::TransformContext>,
        request: &HttpRequest,
    ) -> std::result::Result<Self, String> {
        if !request.method.eq_ignore_ascii_case("POST") {
            return Err("Subhub only authorizes POST requests to provider inference APIs".into());
        }
        let host = normalize_https_host(&request.host)
            .ok_or_else(|| "Iron request used an unsupported host or port".to_string())?;
        let sni = context
            .map(|context| context.sni.as_str())
            .unwrap_or_default();
        if !sni.eq_ignore_ascii_case(&host) {
            return Err("Iron request SNI did not match its provider host".into());
        }
        let path = request_target(&request.url, &host)?;
        let route_path = path.split('?').next().unwrap_or(path.as_str());
        let provider = match (host.as_str(), route_path) {
            ("api.anthropic.com", "/v1/messages") => Provider::Claude,
            ("chatgpt.com", "/backend-api/codex/responses") => Provider::Codex,
            _ => return Err("request is not an approved Subhub provider endpoint".into()),
        };
        Ok(Self {
            provider,
            host,
            method: "POST".into(),
            path,
        })
    }
}

fn normalize_https_host(authority: &str) -> Option<String> {
    let authority = authority.trim().to_ascii_lowercase();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        if port != "443" || host.is_empty() || host.contains(':') {
            return None;
        }
        Some(host.into())
    } else {
        Some(authority)
    }
}

fn request_target(url: &str, expected_host: &str) -> std::result::Result<String, String> {
    if url.starts_with('/') {
        if url.starts_with("//") || url.contains('#') {
            return Err("Iron request used an invalid origin-form target".into());
        }
        return Ok(url.to_owned());
    }
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| "Iron request URL was neither absolute nor origin-form".to_string())?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some(expected_host)
        || parsed.port_or_known_default() != Some(443)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("Iron request URL did not match its HTTPS provider host".into());
    }
    let mut target = parsed.path().to_owned();
    if let Some(query) = parsed.query() {
        target.push('?');
        target.push_str(query);
    }
    Ok(target)
}

fn proto_headers_to_http(
    headers: &HashMap<String, HeaderValues>,
) -> std::result::Result<HeaderMap, String> {
    let mut output = HeaderMap::new();
    for (name, values) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "Iron request contained an invalid header name".to_string())?;
        for value in &values.values {
            let value = HeaderValue::from_str(value)
                .map_err(|_| "Iron request contained an invalid header value".to_string())?;
            output.append(name.clone(), value);
        }
    }
    Ok(output)
}

fn http_headers_to_proto(headers: &HeaderMap) -> HashMap<String, HeaderValues> {
    let mut output: HashMap<String, HeaderValues> = HashMap::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            output
                .entry(name.as_str().into())
                .or_default()
                .values
                .push(value.into());
        }
    }
    output
}

fn proto_header(headers: &HashMap<String, HeaderValues>, wanted: &str) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .and_then(|(_, values)| values.values.first())
        .cloned()
}

fn reject(status: StatusCode, message: &str) -> TransformRequestResponse {
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "error",
        "error": {"type": "subhub_error", "message": message}
    }))
    .unwrap_or_default();
    TransformRequestResponse {
        action: TransformAction::Reject as i32,
        response: Some(HttpResponse {
            status_code: i32::from(status.as_u16()),
            headers: HashMap::from([(
                "content-type".into(),
                HeaderValues {
                    values: vec!["application/json".into()],
                },
            )]),
            body,
        }),
        modified_request: None,
        annotations: HashMap::from([("rejected".into(), "true".into())]),
    }
}

fn new_traceparent() -> String {
    let mut rng = rand::rng();
    loop {
        let trace_id: [u8; 16] = rng.random();
        let span_id: [u8; 8] = rng.random();
        if trace_id.iter().any(|byte| *byte != 0) && span_id.iter().any(|byte| *byte != 0) {
            return format!(
                "00-{}-{}-01",
                lowercase_hex(&trace_id),
                lowercase_hex(&span_id)
            );
        }
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn valid_traceparent(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 55 || bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return false;
    }
    let version = &value[0..2];
    let trace_id = &value[3..35];
    let span_id = &value[36..52];
    let flags = &value[53..55];
    version != "ff"
        && [version, trace_id, span_id, flags]
            .into_iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && trace_id.bytes().any(|byte| byte != b'0')
        && span_id.bytes().any(|byte| byte != b'0')
}

#[cfg(test)]
mod tests {
    use super::super::super::protocol::CredentialUsage;
    use super::super::super::state::CredentialHealth;
    use super::super::super::state::test_state;
    use super::super::proto::TransformContext;
    use super::super::proto::transform_service_server::TransformService;
    use super::*;
    use crate::provider::StoredCredential;

    fn claude_request() -> TransformRequestRequest {
        TransformRequestRequest {
            context: Some(TransformContext {
                sni: "api.anthropic.com".into(),
                client_cert_der: Vec::new(),
                tunnel: None,
            }),
            request: Some(HttpRequest {
                method: "POST".into(),
                url: "/v1/messages".into(),
                headers: HashMap::from([
                    (
                        "authorization".into(),
                        HeaderValues {
                            values: vec!["Bearer placeholder".into()],
                        },
                    ),
                    (
                        "x-request-id".into(),
                        HeaderValues {
                            values: vec!["request-1".into()],
                        },
                    ),
                ]),
                body: br#"{"model":"claude-sonnet","messages":[]}"#.to_vec(),
                host: "api.anthropic.com".into(),
                remote_addr: "127.0.0.1:10000".into(),
            }),
        }
    }

    #[tokio::test]
    async fn request_transform_injects_selected_credential_and_correlation() {
        let state = test_state();
        let service = IronTransform::new(state.clone());
        let response = service
            .transform_request(Request::new(claude_request()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.action, TransformAction::Continue as i32);
        let modified = response.modified_request.unwrap();
        assert_eq!(
            proto_header(&modified.headers, "authorization").as_deref(),
            Some("Bearer secret-b")
        );
        assert_eq!(
            proto_header(&modified.headers, "x-request-id").as_deref(),
            Some("request-1")
        );
        let traceparent = proto_header(&modified.headers, "traceparent").unwrap();
        assert!(valid_traceparent(&traceparent));
        assert_eq!(state.iron_attempts.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn request_transform_rejects_unapproved_destinations_before_selection() {
        let state = test_state();
        let service = IronTransform::new(state);
        let mut request = claude_request();
        request.request.as_mut().unwrap().host = "attacker.example".into();
        let response = service
            .transform_request(Request::new(request))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.action, TransformAction::Reject as i32);
        assert_eq!(response.response.unwrap().status_code, 403);
    }

    #[tokio::test]
    async fn codex_transform_replaces_token_and_account_as_one_identity() {
        let state = test_state();
        *state.credentials.write().await = vec![StoredCredential {
            name: "codex-ready".into(),
            access_token: "codex-secret".into(),
            expires_at: None,
            scopes: Vec::new(),
            provider: Provider::Codex,
            account_id: Some("account-ready".into()),
            refresh_error: None,
        }];
        *state.health.write().await = HashMap::from([(
            "codex-ready".into(),
            CredentialHealth {
                usage: Some(CredentialUsage::Codex(
                    serde_json::from_value(serde_json::json!({
                        "rate_limit": {
                            "primary_window": {"used_percent": 10.0, "reset_at": 1}
                        }
                    }))
                    .unwrap(),
                )),
                error: None,
                checked_at: 1,
            },
        )]);
        let service = IronTransform::new(state);
        let response = service
            .transform_request(Request::new(TransformRequestRequest {
                context: Some(TransformContext {
                    sni: "chatgpt.com".into(),
                    client_cert_der: Vec::new(),
                    tunnel: None,
                }),
                request: Some(HttpRequest {
                    method: "POST".into(),
                    url: "/backend-api/codex/responses".into(),
                    headers: HashMap::from([
                        (
                            "authorization".into(),
                            HeaderValues {
                                values: vec!["Bearer placeholder".into()],
                            },
                        ),
                        (
                            "chatgpt-account-id".into(),
                            HeaderValues {
                                values: vec!["placeholder-account".into()],
                            },
                        ),
                        (
                            "x-codex-client".into(),
                            HeaderValues {
                                values: vec!["preserved".into()],
                            },
                        ),
                    ]),
                    body: br#"{"model":"gpt-5","input":[]}"#.to_vec(),
                    host: "chatgpt.com:443".into(),
                    remote_addr: "127.0.0.1:10000".into(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.action, TransformAction::Continue as i32);
        let headers = response.modified_request.unwrap().headers;
        assert_eq!(
            proto_header(&headers, "authorization").as_deref(),
            Some("Bearer codex-secret")
        );
        assert_eq!(
            proto_header(&headers, "chatgpt-account-id").as_deref(),
            Some("account-ready")
        );
        assert_eq!(
            proto_header(&headers, "openai-beta").as_deref(),
            Some("codex-1")
        );
        assert_eq!(
            proto_header(&headers, "x-codex-client").as_deref(),
            Some("preserved")
        );
    }

    #[test]
    fn request_target_preserves_query_for_retry_correlation() {
        assert_eq!(
            request_target("/v1/messages?beta=1", "api.anthropic.com").unwrap(),
            "/v1/messages?beta=1"
        );
        assert_eq!(
            request_target(
                "https://api.anthropic.com/v1/messages?beta=1",
                "api.anthropic.com"
            )
            .unwrap(),
            "/v1/messages?beta=1"
        );
        assert!(request_target("//attacker.example/path", "api.anthropic.com").is_err());
    }

    #[test]
    fn traceparent_validation_rejects_zero_and_malformed_ids() {
        assert!(valid_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        ));
        assert!(!valid_traceparent(
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
        ));
        assert!(!valid_traceparent("not-a-traceparent"));
    }
}
