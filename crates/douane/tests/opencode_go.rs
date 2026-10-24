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
    let bytes = events
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect::<String>()
        .trim_end()
        .as_bytes()
        .to_vec();
    let chunks: Vec<_> = bytes
        .chunks(7)
        .map(|b| Ok::<_, std::io::Error>(Bytes::copy_from_slice(b)))
        .collect();
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(futures_util::stream::iter(chunks)),
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
}

impl Fixture {
    async fn new() -> Self {
        let upstream = Arc::new(Upstream::default());
        let server = serve(
            Router::new()
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
                json!({"provider":"opencode_go","access_token":"mock-go-key",
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
        }
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
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
async fn muse_uses_responses_for_every_client_and_tool_followup() {
    let fixture = Fixture::new().await;
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
            .any(|m| m["id"] == format!("opencode_go/{MUSE}"))
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
            assert_eq!(sent.headers["x-opencode-session"], "stable-muse-session");
            assert_eq!(sent.body["model"], MUSE);
            assert_eq!(sent.body["tools"][0]["name"], "read");
            if kind != "messages" {
                assert_eq!(sent.body["prompt_cache_key"], "muse-cache");
                assert_eq!(sent.body["reasoning"]["effort"], "high");
            }
            if kind == "responses" {
                let mut expected = request(kind, false, follow);
                expected["model"] = json!(MUSE);
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
    let fixture = Fixture::new().await;
    for kind in ["messages", "chat/completions", "responses"] {
        let response = fixture.post(kind, request(kind, true, false)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let text = response.text().await.unwrap();
        assert!(text.contains(TEXT), "{kind}: {text}");
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
    let fixture = Fixture::new().await;
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
        if kind == "responses" {
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
    let fixture = Fixture::new().await;
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
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let fixture = Fixture::new().await;
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
        assert_eq!(sent.headers["x-opencode-session"], "stable-muse-session");
        assert_eq!(sent.body["prompt_cache_key"], "muse-cache");
        assert_eq!(sent.body["reasoning"]["effort"], "high");
        if follow {
            assert_eq!(sent.body["input"][1], output()[0]);
            assert_eq!(sent.body["input"][4]["call_id"], "call_1");
        }
    }
    socket.close(None).await.unwrap();
}
