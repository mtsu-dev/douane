//! OpenCode Go's Responses-only Muse route, without subscription traffic.
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use douane::{settings::Config, state::AppState};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

const MUSE: &str = "muse-spark-1.3-contributor";
const TEXT: &str = "Salut 😀 café";

#[derive(Clone)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Default)]
struct Upstream {
    seen: Mutex<Vec<Seen>>,
    mode: Mutex<String>,
}

struct Server {
    url: String,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(router: Router) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { url, task }
}

fn output() -> Value {
    json!([
        {"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque-muse-signature"},
        {"id":"msg_1","type":"message","role":"assistant","status":"completed",
         "content":[{"type":"output_text","text":TEXT,"annotations":[]}]},
        {"id":"fc_1","type":"function_call","call_id":"call_1","name":"read",
         "arguments":"{\"path\":\"a.rs\"}","status":"completed"}
    ])
}

async fn answer(
    State(upstream): State<Arc<Upstream>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    upstream.seen.lock().unwrap().push(Seen {
        path: uri.path().to_owned(),
        headers,
        body: body.clone(),
    });
    if body["model"] == MUSE && !uri.path().ends_with("/responses") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"Model does not support this protocol"}})),
        )
            .into_response();
    }
    if body["model"] != MUSE {
        return Json(json!({"id":"chat_1","object":"chat.completion","model":"glm-5.3-flash",
            "choices":[{"index":0,"message":{"role":"assistant","content":TEXT},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":2}})).into_response();
    }
    let mode = upstream.mode.lock().unwrap().clone();
    if mode == "http-error" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"invalid fixture input"}})),
        )
            .into_response();
    }
    let response = json!({"id":"resp_muse","object":"response","status":"completed","model":MUSE,
        "output":output(),"usage":{"input_tokens":10,"output_tokens":2,
        "input_tokens_details":{"cached_tokens":4}}});
    if body["stream"] != true {
        return Json(response).into_response();
    }
    let mut events = vec![
        json!({"type":"response.subscription_usage","subscription":{"tier":"private-tier", "weekly":{"used_percent":20,"resets_at":1792368000}}}),
        json!({"type":"response.created","response":{"id":"resp_muse","model":MUSE}}),
        json!({"type":"response.output_item.added","output_index":0,"item":output()[0]}),
        json!({"type":"response.output_text.delta","output_index":1,"delta":TEXT}),
        json!({"type":"response.output_item.added","output_index":2,
            "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":"}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"\"a.rs\"}"}),
    ];
    match mode.as_str() {
        "stream-error" => events.push(
            json!({"type":"response.failed","response":{"status":"failed",
            "error":{"code":"server_error","message":"fixture failure"}}}),
        ),
        "truncated" => {}
        _ => events.push(json!({"type":"response.completed","response":response})),
    }
    // Fragment inside UTF-8 and leave the terminal event without a separator.
    let encoded = events
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect::<String>();
    let bytes = if mode == "held-open" || mode == "late-usage" {
        encoded.as_str()
    } else {
        encoded.trim_end()
    }
    .as_bytes()
    .to_vec();
    let chunks: Vec<_> = bytes
        .chunks(7)
        .map(|b| Ok::<_, std::io::Error>(Bytes::copy_from_slice(b)))
        .collect();
    let stream = futures_util::stream::iter(chunks);
    if mode == "late-usage" {
        let tail = futures_util::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            Ok::<_, std::io::Error>(Bytes::from_static(b"event: response.subscription_usage\ndata: {\"type\":\"response.subscription_usage\",\"subscription\":{\"weekly\":{\"used_percent\":55,\"resets_at\":1792368000}}}\n\n"))
        });
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(stream.chain(tail).chain(futures_util::stream::pending())),
        )
            .into_response();
    }
    if mode == "held-open" {
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(stream.chain(futures_util::stream::pending())),
        )
            .into_response();
    }
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

struct Fixture {
    state: Arc<AppState>,
    upstream: Arc<Upstream>,
    gateway: Server,
    _server: Server,
    _directory: tempfile::TempDir,
    client: reqwest::Client,
    key: String,
    provider: String,
}

impl Fixture {
    async fn new(provider: &str) -> Self {
        let upstream = Arc::new(Upstream::default());
        let server = serve(
            Router::new()
                .route("/muse-code/models", get(|| async { Json(json!({"data":[
                    {"id":MUSE,"metadata":{"muse-code":{"name":"Muse Spark","family":"avocado","release_date":"2026-09-02","limit":{"context":1007997,"output":128000},"options":{},"variants":{"high":{"reasoningEffort":"high"}}}}}
                ]})) }))
                .route(
                    "/v1/models",
                    get(|| async { Json(json!({"data":[{"id":MUSE},{"id":"glm-5.3-flash"}]})) }),
                )
                .route("/v1/responses", post(answer))
                .route("/v1/chat/completions", post(answer))
                .with_state(upstream.clone()),
        )
        .await;
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("accounts");
        config.server.database = directory.path().join("calls.sqlite");
        let state = AppState::new(config, None).unwrap();
        let (_, key) = state
            .clients
            .create(douane::clients::ClientPatch {
                name: Some("tests".into()),
                ..Default::default()
            })
            .unwrap();
        state
            .accounts
            .import(
                json!({"provider":provider,"access_token":"mock-go-key",
            "base_url":server.url}),
                None,
            )
            .await
            .unwrap();
        state.refresh_models().await;
        let gateway = serve(douane::router(state.clone())).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .default_headers(
                [(
                    "authorization".parse().unwrap(),
                    format!("Bearer {key}").parse().unwrap(),
                )]
                .into_iter()
                .collect(),
            )
            .build()
            .unwrap();
        Self {
            state,
            upstream,
            gateway,
            _server: server,
            _directory: directory,
            client,
            key,
            provider: provider.into(),
        }
    }

    async fn post(&self, path: &str, mut body: Value) -> reqwest::Response {
        if self.provider == "meta" {
            body["model"] = json!(body["model"].as_str().unwrap().replacen(
                "opencode_go/",
                "meta/",
                1
            ));
        }
        self.client
            .post(format!("{}/v1/{path}", self.gateway.url))
            .header("session_id", "stable-muse-session")
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

fn request(kind: &str, stream: bool, follow: bool) -> Value {
    let model = format!("opencode_go/{MUSE}");
    match kind {
        "responses" => json!({"model":model,"stream":stream,"prompt_cache_key":"muse-cache",
            "reasoning":{"effort":"high"},"include":["reasoning.encrypted_content"],
            "input":if follow { json!([
                {"role":"user","content":"read the file"},output()[0],output()[1],output()[2],
                {"type":"function_call_output","call_id":"call_1","output":"file content"}
            ]) } else { json!([{"role":"user","content":"read the file"}]) },
            "tools":[{"type":"function","name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}]}),
        "messages" => json!({"model":model,"stream":stream,"max_tokens":32,
            "system":[{"type":"text","text":"system instruction","cache_control":{"type":"ephemeral"}}],
            "messages":if follow { json!([
                {"role":"user","content":"read the file"},
                {"role":"assistant","content":[{"type":"text","text":TEXT},{"type":"tool_use","id":"call_1","name":"read","input":{"path":"a.rs"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"file content"}]}
            ]) } else { json!([{"role":"user","content":"read the file"}]) },
            "tools":[{"name":"read","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}]}),
        _ => json!({"model":model,"stream":stream,"max_completion_tokens":32,
            "prompt_cache_key":"muse-cache","reasoning_effort":"high",
            "messages":if follow { json!([
                {"role":"user","content":"read the file"},
                {"role":"assistant","content":TEXT,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a.rs\"}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"file content"}
            ]) } else { json!([{"role":"user","content":"read the file"}]) },
            "tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]}),
    }
}

#[tokio::test]
async fn native_muse_routes_require_a_client_key_and_stay_in_meta() {
    let fixture = Fixture::new("meta").await;
    for path in ["models", "responses"] {
        let url = format!("{}/muse-code/{path}", fixture.gateway.url);
        let request = if path == "models" {
            reqwest::Client::new().get(&url)
        } else {
            reqwest::Client::new()
                .post(&url)
                .json(&request("responses", true, false))
        };
        assert_eq!(request.send().await.unwrap().status(), 401, "{path}");
    }
    for follow in [false, true] {
        let mut body = request("responses", true, follow);
        body["model"] = json!(MUSE);
        let response = fixture
            .client
            .post(format!("{}/muse-code/responses", fixture.gateway.url))
            .header("session_id", "stable-muse-session")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.contains("response.completed") && text.contains(TEXT));
        let sent = fixture
            .upstream
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(sent.path, "/v1/responses");
        assert_eq!(sent.body["model"], MUSE);
        assert_eq!(sent.headers["session_id"], "stable-muse-session");
        if follow {
            assert_eq!(sent.body["input"][1], output()[0]);
        }
    }
    let catalogue: Value = fixture
        .client
        .get(format!(
            "{}/muse-code/models?provider=opencode_go",
            fixture.gateway.url
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(catalogue["data"].as_array().unwrap().len(), 1);
    assert_eq!(catalogue["data"][0]["id"], MUSE);
    assert_eq!(catalogue["data"][0]["owned_by"], "meta");
    assert_eq!(
        catalogue["data"][0]["metadata"]["muse-code"]["limit"]["context"],
        1007997
    );
    fixture
        .state
        .models
        .write()
        .await
        .iter_mut()
        .find(|m| m.upstream_id == MUSE)
        .unwrap()
        .enabled = false;
    let catalogue: Value = fixture
        .client
        .get(format!("{}/muse-code/models", fixture.gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(catalogue["data"].as_array().unwrap().is_empty());
    let go = Fixture::new("opencode_go").await;
    let response = go
        .client
        .post(format!("{}/muse-code/responses", go.gateway.url))
        .json(&request("responses", true, false))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        400,
        "an explicit aggregator prefix cannot escape Meta"
    );
    let mut body = request("responses", true, false);
    body["model"] = json!(MUSE);
    let response = go
        .client
        .post(format!("{}/muse-code/responses", go.gateway.url))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert!(go.upstream.seen.lock().unwrap().is_empty());
    let catalogue: Value = go
        .client
        .get(format!("{}/muse-code/models", go.gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(catalogue["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn muse_uses_responses_for_every_client_and_tool_followup() {
    for provider in ["opencode_go", "meta"] {
        muse_uses_responses_for_every_client_and_tool_followup_for(provider).await;
    }
}

async fn muse_uses_responses_for_every_client_and_tool_followup_for(provider: &str) {
    let fixture = Fixture::new(provider).await;
    let models: Value = fixture
        .client
        .get(format!("{}/v1/models", fixture.gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == format!("{provider}/{MUSE}"))
    );
    for kind in ["messages", "chat/completions", "responses"] {
        for follow in [false, true] {
            let response = fixture.post(kind, request(kind, false, follow)).await;
            let status = response.status();
            let result: Value = response.json().await.unwrap();
            assert_eq!(status, StatusCode::OK, "{kind}, follow={follow}: {result}");
            match kind {
                "messages" => {
                    assert_eq!(result["stop_reason"], "tool_use");
                    assert_eq!(result["content"][0]["text"], TEXT);
                    assert_eq!(result["content"][1]["id"], "call_1");
                    assert_eq!(result["content"][1]["input"]["path"], "a.rs");
                    assert_eq!(result["usage"]["input_tokens"], 6);
                    assert_eq!(result["usage"]["cache_read_input_tokens"], 4);
                }
                "responses" => assert_eq!(result["output"], output()),
                _ => {
                    assert_eq!(result["choices"][0]["message"]["content"], TEXT);
                    assert_eq!(
                        result["choices"][0]["message"]["tool_calls"][0]["id"],
                        "call_1"
                    );
                    assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
                }
            }
            let sent = fixture
                .upstream
                .seen
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .clone();
            assert_eq!(sent.path, "/v1/responses");
            assert_eq!(sent.headers["authorization"], "Bearer mock-go-key");
            assert_eq!(sent.headers["session_id"], "stable-muse-session");
            if provider == "opencode_go" {
                assert_eq!(sent.headers["x-opencode-session"], "stable-muse-session");
            } else {
                assert_eq!(sent.headers["x-client-id"], "tbh:tui");
                assert!(
                    sent.headers["user-agent"]
                        .to_str()
                        .unwrap()
                        .starts_with("muse-build/1.3.0")
                );
                assert_eq!(sent.body["stream"], true);
            }
            assert_eq!(sent.body["model"], MUSE);
            assert_eq!(sent.body["tools"][0]["name"], "read");
            if kind != "messages" {
                assert_eq!(sent.body["prompt_cache_key"], "muse-cache");
                assert_eq!(sent.body["reasoning"]["effort"], "high");
            }
            if kind == "responses" {
                let mut expected = request(kind, false, follow);
                expected["model"] = json!(MUSE);
                if provider == "meta" {
                    expected["stream"] = json!(true);
                    expected["instructions"] = json!("");
                }
                assert_eq!(
                    sent.body, expected,
                    "native reasoning and cache fields must stay opaque"
                );
            } else {
                assert_eq!(sent.body["max_output_tokens"], 32);
            }
            if follow {
                let items = sent.body["input"].as_array().unwrap();
                assert!(
                    items
                        .iter()
                        .any(|i| i["type"] == "function_call" && i["call_id"] == "call_1")
                );
                assert!(items.iter().any(|i| i["type"] == "function_call_output"
                    && i["call_id"] == "call_1"
                    && i["output"] == "file content"));
            }
        }
    }
    if provider == "meta" {
        let account = fixture.state.accounts.list().await.remove(0);
        let snapshot = fixture.state.quotas.get(&account.id).await.unwrap();
        assert_eq!(snapshot.windows[0].remaining_percent, Some(80.0));
        assert!(snapshot.error.is_none());
        let config = fixture.state.config.read().await.clone();
        let reloaded = AppState::new(config, None).unwrap();
        assert_eq!(
            reloaded.quotas.get(&account.id).await.unwrap().windows[0].remaining_percent,
            Some(80.0)
        );
    }
    assert!(
        fixture
            .state
            .accounts
            .list()
            .await
            .iter()
            .all(|a| a.last_error.is_none())
    );
}

#[tokio::test]
async fn muse_streams_text_tools_and_unterminated_completion_for_all_clients() {
    for provider in ["opencode_go", "meta"] {
        muse_streams_text_tools_and_unterminated_completion_for_all_clients_for(provider).await;
    }
}

async fn muse_streams_text_tools_and_unterminated_completion_for_all_clients_for(provider: &str) {
    let fixture = Fixture::new(provider).await;
    for kind in ["messages", "chat/completions", "responses"] {
        let response = fixture.post(kind, request(kind, true, false)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let text = response.text().await.unwrap();
        assert!(text.contains(TEXT), "{kind}: {text}");
        if provider == "meta" {
            assert!(!text.contains("subscription_usage") && !text.contains("private-tier"));
        }
        assert!(
            text.contains("call_1") && text.contains("read"),
            "{kind}: {text}"
        );
        assert!(!text.contains("event: error"), "{kind}: {text}");
        let terminal = match kind {
            "messages" => "message_stop",
            "responses" => "response.completed",
            _ => "[DONE]",
        };
        assert!(text.contains(terminal), "{kind}: {text}");
        let mut frames = douane::translate::Frames::default();
        let mut events = frames.feed(text.as_bytes()).unwrap();
        events.extend(frames.finish().unwrap());
        let arguments: String = events
            .iter()
            .filter_map(|event| match kind {
                "messages" if event["delta"]["type"] == "input_json_delta" => {
                    event["delta"]["partial_json"].as_str()
                }
                "responses" if event["type"] == "response.function_call_arguments.delta" => {
                    event["delta"].as_str()
                }
                "chat/completions" => {
                    event["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).unwrap(),
            json!({"path":"a.rs"}),
            "{kind}"
        );
        if kind == "responses" {
            assert!(text.contains("opaque-muse-signature"));
        }
        if kind == "messages" {
            assert!(text.contains("cache_read_input_tokens\":4"));
        }
    }
}

#[tokio::test]
async fn muse_errors_and_disconnections_do_not_become_successful_completions() {
    for provider in ["opencode_go", "meta"] {
        muse_errors_and_disconnections_do_not_become_successful_completions_for(provider).await;
    }
}

async fn muse_errors_and_disconnections_do_not_become_successful_completions_for(provider: &str) {
    let fixture = Fixture::new(provider).await;
    for kind in ["messages", "chat/completions", "responses"] {
        *fixture.upstream.mode.lock().unwrap() = "http-error".into();
        let response = fixture.post(kind, request(kind, false, false)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = response.json().await.unwrap();
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
        if kind == "responses" && provider == "opencode_go" {
            assert_eq!(error["error"]["message"], "invalid fixture input");
        }
        for mode in ["stream-error", "truncated"] {
            *fixture.upstream.mode.lock().unwrap() = mode.into();
            let response = fixture.post(kind, request(kind, true, false)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let text = response.text().await.unwrap();
            assert!(
                text.contains("response.failed") || text.contains("event: error"),
                "{kind}/{mode}: {text}"
            );
            assert!(
                !text.contains("response.completed")
                    && !text.contains("message_stop")
                    && !text.contains("[DONE]"),
                "{kind}/{mode}: {text}"
            );
        }
    }
}

#[tokio::test]
async fn other_opencode_go_models_keep_chat_completions() {
    let provider = "opencode_go";
    let fixture = Fixture::new(provider).await;
    for kind in ["messages", "chat/completions", "responses"] {
        let mut body = request(kind, false, false);
        body["model"] = json!("opencode_go/glm-5.3-flash");
        assert_eq!(fixture.post(kind, body).await.status(), StatusCode::OK);
        assert_eq!(
            fixture.upstream.seen.lock().unwrap().last().unwrap().path,
            "/v1/chat/completions"
        );
    }
}

#[tokio::test]
async fn muse_codex_websocket_uses_http_responses_and_preserves_native_items() {
    for provider in ["opencode_go", "meta"] {
        muse_codex_websocket_uses_http_responses_and_preserves_native_items_for(provider).await;
    }
}

async fn muse_codex_websocket_uses_http_responses_and_preserves_native_items_for(provider: &str) {
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let fixture = Fixture::new(provider).await;
    let mut upgrade = format!(
        "{}/v1/responses",
        fixture.gateway.url.replace("http://", "ws://")
    )
    .into_client_request()
    .unwrap();
    upgrade.headers_mut().insert(
        "authorization",
        format!("Bearer {}", fixture.key).parse().unwrap(),
    );
    upgrade
        .headers_mut()
        .insert("session_id", "stable-muse-session".parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(upgrade).await.unwrap();
    for follow in [false, true] {
        let mut body = request("responses", true, follow);
        body["model"] = json!(format!("{provider}/{MUSE}"));
        body["type"] = json!("response.create");
        socket
            .send(Message::Text(body.to_string().into()))
            .await
            .unwrap();
        let events = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut events = vec![];
            loop {
                let event = socket.next().await.unwrap().unwrap();
                if let Message::Text(text) = event {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    assert_ne!(event["type"], "error", "{event}");
                    let done = event["type"] == "response.completed";
                    events.push(event);
                    if done {
                        break;
                    }
                }
            }
            events
        })
        .await
        .expect("Muse must complete through the Codex HTTP bridge");
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "response.output_text.delta" && e["delta"] == TEXT)
        );
        assert_eq!(events.last().unwrap()["response"]["output"], output());
        let sent = fixture
            .upstream
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(sent.path, "/v1/responses");
        if provider == "opencode_go" {
            assert_eq!(sent.headers["x-opencode-session"], "stable-muse-session");
        }
        assert_eq!(sent.body["prompt_cache_key"], "muse-cache");
        assert_eq!(sent.body["reasoning"]["effort"], "high");
        if follow {
            assert_eq!(sent.body["input"][1], output()[0]);
            assert_eq!(sent.body["input"][4]["call_id"], "call_1");
        }
    }
    socket.close(None).await.unwrap();
}

#[tokio::test]
async fn meta_completed_responses_finish_when_upstream_keeps_the_body_open() {
    let fixture = Fixture::new("meta").await;
    *fixture.upstream.mode.lock().unwrap() = "held-open".into();
    for kind in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            let result = tokio::time::timeout(std::time::Duration::from_millis(700), async {
                let response = fixture.post(kind, request(kind, stream, false)).await;
                assert_eq!(response.status(), StatusCode::OK);
                response.text().await.unwrap()
            })
            .await
            .expect("completed Meta response must not wait for EOF");
            assert!(result.contains(TEXT));
        }
    }
}

#[tokio::test]
async fn meta_late_subscription_usage_survives_prompt_client_completion() {
    for stream in [false, true] {
        let fixture = Fixture::new("meta").await;
        *fixture.upstream.mode.lock().unwrap() = "late-usage".into();
        fixture
            .post("responses", request("responses", stream, false))
            .await
            .text()
            .await
            .unwrap();
        let id = fixture.state.accounts.list().await.remove(0).id;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if fixture
                    .state
                    .quotas
                    .get(&id)
                    .await
                    .is_some_and(|q| q.windows[0].remaining_percent == Some(45.0))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("private quota telemetry must survive the completed client response");
    }
}
