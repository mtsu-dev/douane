//! Real local sockets exercise the public router without subscription traffic.
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{
        OriginalUri, State, WebSocketUpgrade,
        ws::{Message as ServerMessage, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use douane::{ledger::Call, scheduler::Strategy, settings::Config, state::AppState};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const WAIT: Duration = Duration::from_secs(5);
const TEXT: &str = "Salut 😀 café";
type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct Received {
    socket: Option<usize>,
    headers: HeaderMap,
    body: Value,
}

#[derive(Default)]
struct Mock {
    rejected_status: AtomicUsize,
    received: Mutex<Vec<Received>>,
    handshakes: Mutex<Vec<HeaderMap>>,
    connections: AtomicUsize,
    closed: AtomicUsize,
    rejected_token: Mutex<Option<String>>,
    trailing_frame: Mutex<bool>,
    hold_http: AtomicBool,
    closed_http: AtomicUsize,
    native_failure: Mutex<Option<Value>>,
    native_partial: AtomicBool,
    hold_native: AtomicBool,
    native_release: tokio::sync::Notify,
    native_pings: AtomicUsize,
    unique_native_ids: AtomicBool,
    omit_native_lane: AtomicBool,
    omit_content_type: AtomicBool,
    items_before_empty_completion: AtomicBool,
    refusal: Mutex<Option<(u16, Value)>>,
    limit_headers: AtomicBool,
    /// Milliseconds between the HTTP body's chunks; a few by default.
    chunk_pause_ms: AtomicU64,
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
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server { url, task }
}

struct Fixture {
    state: Arc<AppState>,
    mock: Arc<Mock>,
    gateway: Server,
    upstream: Server,
    client: reqwest::Client,
    /// The douane client key every call presents.
    key: String,
    accounts: Vec<(String, String, String)>,
    _directory: TempDir,
}

impl Fixture {
    async fn new(providers: &[(&str, &str)], strategy: Strategy) -> Self {
        let mock = Arc::new(Mock::default());
        let upstream = serve(
            Router::new()
                .route("/responses", post(mock_http).get(mock_upgrade))
                .route("/v1/responses", post(mock_http).get(mock_upgrade))
                .route("/v1/chat/completions", post(mock_http))
                .route(
                    "/v1/messages/count_tokens",
                    post(|| async { axum::Json(json!({"input_tokens":12})) }),
                )
                .route(
                    "/v1/models",
                    get(|| async { axum::Json(json!({"data":[{"id":"gpt-test"}]})) }),
                )
                .with_state(mock.clone()),
        )
        .await;
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("accounts");
        config.server.database = directory.path().join("calls.sqlite");
        config.settings.strategy = strategy;
        let state = AppState::new(config, None).unwrap();
        let (_, key) = state
            .clients
            .create(douane::clients::ClientPatch {
                name: Some("tests".into()),
                ..Default::default()
            })
            .unwrap();
        let mut accounts = vec![];
        for (provider, name) in providers {
            let summary = state
                .accounts
                .import(account(provider, name, &upstream.url), Some((*name).into()))
                .await
                .unwrap();
            accounts.push((summary.id, (*provider).into(), (*name).into()));
        }
        state.refresh_models().await;
        let gateway = serve(douane::router(state.clone())).await;
        Self {
            state,
            mock,
            gateway,
            upstream,
            client: reqwest::Client::builder()
                .timeout(WAIT)
                .default_headers(
                    [(
                        reqwest::header::AUTHORIZATION,
                        format!("Bearer {key}").parse().unwrap(),
                    )]
                    .into_iter()
                    .collect(),
                )
                .build()
                .unwrap(),
            key,
            accounts,
            _directory: directory,
        }
    }

    async fn socket(&self, session: &str) -> ClientSocket {
        let url = format!(
            "{}/v1/responses",
            self.gateway.url.replace("http://", "ws://")
        );
        let mut request = url.into_client_request().unwrap();
        let bearer = format!("Bearer {}", self.key);
        for (name, value) in [
            ("session_id", session),
            ("thread-id", session),
            ("authorization", bearer.as_str()),
            ("x-api-key", self.key.as_str()),
            ("x-douane-admin", "worthless-admin-key"),
            ("x-management-key", "worthless-management-key"),
            ("chatgpt-account-id", "worthless-caller-account"),
            ("cookie", "caller-cookie=fixture"),
            ("openai-beta", "responses_websockets=2026-02-06"),
        ] {
            request.headers_mut().insert(name, value.parse().unwrap());
        }
        timeout(WAIT, connect_async(request))
            .await
            .unwrap()
            .unwrap()
            .0
    }

    async fn post(&self, path: &str, session: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.gateway.url))
            .header("session_id", session)
            .header("session-id", session)
            .header("x-opencode-session", session)
            .header("thread-id", session)
            .header("authorization", format!("Bearer {}", self.key))
            .header("x-api-key", &self.key)
            .header("x-douane-admin", "worthless-admin-key")
            .header("x-management-key", "worthless-management-key")
            .header("chatgpt-account-id", "worthless-caller-account")
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    fn received(&self) -> Vec<Received> {
        self.mock.received.lock().unwrap().clone()
    }

    async fn calls(&self, count: usize) -> Vec<Call> {
        timeout(WAIT, async {
            loop {
                if self.state.ledger.requests.load(Ordering::Relaxed) >= count as u64 {
                    self.state.ledger.flush().await;
                    let calls = self.state.ledger.calls(100).await;
                    if calls.len() >= count {
                        return calls;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("completed calls must reach the durable ledger")
    }

    async fn closed(&self, count: usize) {
        timeout(WAIT, async {
            while self.mock.closed.load(Ordering::Relaxed) < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closing the client must close its upstream connection");
    }
}

fn account(provider: &str, name: &str, base: &str) -> Value {
    json!({"type":provider,"access_token":format!("fixture-{name}"),"account_id":format!("identity-{name}"),"base_url":base,"models":["gpt-test"]})
}

fn completed(stream_id: Option<&Value>) -> Value {
    let mut event = json!({"type":"response.completed","response":{"id":"response-fixture","object":"response","status":"completed","model":"gpt-test","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":TEXT}]}],"usage":{"input_tokens":100,"output_tokens":10,"input_tokens_details":{"cached_tokens":80}}}});
    if let Some(id) = stream_id {
        event["stream_id"] = id.clone();
    }
    event
}

async fn mock_http(
    State(mock): State<Arc<Mock>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    mock.received.lock().unwrap().push(Received {
        socket: None,
        headers: headers.clone(),
        body,
    });
    if mock
        .rejected_token
        .lock()
        .unwrap()
        .as_deref()
        .is_some_and(|token| {
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(token)
        })
    {
        return (
            StatusCode::from_u16(mock.rejected_status.load(Ordering::Relaxed) as u16)
                .unwrap_or(StatusCode::TOO_MANY_REQUESTS),
            [("retry-after", "60")],
            "fixture quota reached",
        )
            .into_response();
    }
    if let Some((status, body)) = mock.refusal.lock().unwrap().clone() {
        return (
            StatusCode::from_u16(status).unwrap(),
            [
                ("x-codex-primary-used-percent", "42"),
                ("request-id", "fixture-request"),
                ("set-cookie", "upstream-session=fixture"),
            ],
            axum::Json(body),
        )
            .into_response();
    }
    let mut data = if uri.path().ends_with("chat/completions") {
        [json!({"id":"chat-fixture","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
         json!({"id":"chat-fixture","choices":[{"index":0,"delta":{"content":TEXT},"finish_reason":null}]}),
         json!({"id":"chat-fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
         json!({"id":"chat-fixture","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":80}}})]
            .iter().map(|frame| format!("data: {frame}\n\n")).collect::<String>()
    } else if mock.items_before_empty_completion.load(Ordering::Relaxed) {
        // Codex's backend: the finished items come one by one, then a
        // completion whose output is empty.
        let mut done = completed(None);
        let item = done["response"]["output"][0].take();
        done["response"]["output"] = json!([]);
        format!(
            "event: response.created\ndata: {}\n\nevent: response.output_text.delta\ndata: {}\n\nevent: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}",
            json!({"type":"response.created","response":{"id":"response-fixture"}}),
            json!({"type":"response.output_text.delta","delta":TEXT}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            done
        )
    } else {
        format!(
            "event: response.created\ndata: {}\n\nevent: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}",
            json!({"type":"response.created","response":{"id":"response-fixture"}}),
            json!({"type":"response.output_text.delta","delta":TEXT}),
            completed(None)
        )
    };
    if !*mock.trailing_frame.lock().unwrap() {
        data.push_str("\n\n");
    }
    // Separate HTTP body chunks split a Unicode scalar and the completion JSON.
    let emoji = data.find('😀').unwrap();
    let cuts = [0, emoji + 1, emoji + 3, data.len() - 17, data.len()];
    let chunks: VecDeque<_> = cuts
        .windows(2)
        .map(|cut| Bytes::copy_from_slice(&data.as_bytes()[cut[0]..cut[1]]))
        .collect();
    let hold = mock.hold_http.load(Ordering::Relaxed);
    let omit_content_type = mock.omit_content_type.load(Ordering::Relaxed);
    let probe_limits = mock.limit_headers.load(Ordering::Relaxed);
    // The first chunk comes at once: the pause is the silence inside the body.
    let pause = Duration::from_millis(mock.chunk_pause_ms.load(Ordering::Relaxed).max(3));
    let total = chunks.len();
    let probe = HoldProbe { mock, hold };
    let stream =
        futures_util::stream::unfold((chunks, probe), move |(mut chunks, probe)| async move {
            if probe.hold && chunks.len() <= 2 {
                std::future::pending::<()>().await;
            }
            let chunk = chunks.pop_front()?;
            if chunks.len() < total {
                tokio::time::sleep(pause).await;
            }
            Some((Ok::<_, std::io::Error>(chunk), (chunks, probe)))
        });
    if omit_content_type {
        return Body::from_stream(stream).into_response();
    }
    let mut response = (
        [("content-type", "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response();
    if probe_limits {
        response
            .headers_mut()
            .insert("x-codex-primary-used-percent", "17".parse().unwrap());
        response
            .headers_mut()
            .insert("set-cookie", "upstream-session=fixture".parse().unwrap());
    }
    response
}

struct HoldProbe {
    mock: Arc<Mock>,
    hold: bool,
}
impl Drop for HoldProbe {
    fn drop(&mut self) {
        if self.hold {
            self.mock.closed_http.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn mock_upgrade(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let id = mock.connections.fetch_add(1, Ordering::Relaxed);
    mock.handshakes.lock().unwrap().push(headers.clone());
    upgrade.on_upgrade(move |socket| mock_socket(socket, mock, headers, id))
}

async fn mock_socket(mut socket: WebSocket, mock: Arc<Mock>, headers: HeaderMap, id: usize) {
    while let Some(Ok(message)) = socket.recv().await {
        match message {
            ServerMessage::Text(text) => {
                let body: Value = serde_json::from_str(&text).unwrap();
                mock.received.lock().unwrap().push(Received {
                    socket: Some(id),
                    headers: headers.clone(),
                    body: body.clone(),
                });
                let failure = mock.native_failure.lock().unwrap().clone().filter(|_| {
                    mock.rejected_token.lock().unwrap().as_deref()
                        == headers.get("authorization").and_then(|v| v.to_str().ok())
                });
                if body["type"] == "response.create" {
                    if let Some(mut failure) = failure {
                        if mock.hold_native.swap(false, Ordering::Relaxed) {
                            mock.native_release.notified().await;
                        }
                        if let Some(lane) = body
                            .get("stream_id")
                            .filter(|_| !mock.omit_native_lane.load(Ordering::Relaxed))
                        {
                            failure["stream_id"] = lane.clone();
                        }
                        if mock.native_partial.load(Ordering::Relaxed) {
                            for mut event in [
                                json!({"type":"response.created","response":{"id":"failed-response"}}),
                                json!({"type":"response.output_text.delta","delta":"already emitted"}),
                            ] {
                                if let Some(lane) = body.get("stream_id") {
                                    event["stream_id"] = lane.clone();
                                }
                                if socket
                                    .send(ServerMessage::Text(event.to_string().into()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                        if socket
                            .send(ServerMessage::Text(failure.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                }
                let event = if body["type"] == "response.create" {
                    let mut event = completed(body.get("stream_id"));
                    if mock.unique_native_ids.load(Ordering::Relaxed) {
                        event["response"]["id"] = json!(format!("response-socket-{id}"));
                    }
                    event
                } else {
                    json!({"type":"fixture.cancel.received","stream_id":body["stream_id"],"response_id":body["response_id"]})
                };
                if socket
                    .send(ServerMessage::Text(event.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                if body["fixture_close"] == true {
                    let _ = socket.send(ServerMessage::Close(None)).await;
                    break;
                }
            }
            ServerMessage::Ping(data) => {
                if socket.send(ServerMessage::Pong(data)).await.is_err() {
                    break;
                }
                mock.native_pings.fetch_add(1, Ordering::Relaxed);
            }
            ServerMessage::Close(_) => break,
            _ => {}
        }
    }
    mock.closed.fetch_add(1, Ordering::Relaxed);
}

async fn send(socket: &mut ClientSocket, body: Value) {
    timeout(WAIT, socket.send(Message::Text(body.to_string().into())))
        .await
        .unwrap()
        .unwrap();
}

async fn next(socket: &mut ClientSocket) -> Value {
    timeout(WAIT, async {
        loop {
            match socket
                .next()
                .await
                .expect("socket closed before an event")
                .unwrap()
            {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(data) => {
                    socket.send(Message::Pong(data)).await.unwrap();
                }
                Message::Pong(_) => {}
                message => panic!("unexpected websocket event: {message:?}"),
            }
        }
    })
    .await
    .expect("websocket event timed out")
}

fn assert_private_headers(headers: &HeaderMap, session: &str) {
    assert_eq!(headers.get("session_id").unwrap(), session);
    assert_eq!(headers.get("thread-id").unwrap(), session);
    for name in ["x-api-key", "x-douane-admin", "x-management-key", "cookie"] {
        assert!(
            headers.get(name).is_none(),
            "caller credential {name} leaked upstream"
        );
    }
    assert!(
        headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Bearer fixture-")
    );
    assert!(
        headers
            .get("chatgpt-account-id")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("identity-")
    );
}

#[tokio::test]
async fn websocket_reuses_connection_preserves_cancellation_and_reconnects_to_the_same_account() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::RoundRobin,
    )
    .await;
    let mut socket = fixture.socket("conversation-ws").await;
    for turn in 0..2 {
        send(&mut socket, json!({"type":"response.create","model":"codex/gpt-test","input":[],"stream_id":"lane-1","prompt_cache_key":"conversation-cache","previous_response_id":(turn == 1).then_some("response-fixture")})).await;
        assert_eq!(next(&mut socket).await, completed(Some(&json!("lane-1"))));
    }
    assert_eq!(
        fixture.mock.connections.load(Ordering::Relaxed),
        1,
        "second turn must reuse the native socket"
    );
    let cancel =
        json!({"type":"response.cancel","response_id":"cancelled-response","stream_id":"lane-1"});
    send(&mut socket, cancel.clone()).await;
    assert_eq!(next(&mut socket).await["type"], "fixture.cancel.received");
    socket
        .send(Message::Ping(Bytes::from_static(b"client-ping")))
        .await
        .unwrap();
    let pong = timeout(WAIT, socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(pong, Message::Pong(Bytes::from_static(b"client-ping")));
    send(&mut socket, json!({"type":"response.create","model":"gpt-test","input":[],"stream_id":"lane-2","prompt_cache_key":"conversation-cache","fixture_close":true})).await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    assert!(matches!(
        timeout(WAIT, socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Close(_)
    ));
    fixture.closed(1).await;
    let mut reconnected = fixture.socket("conversation-ws").await;
    send(&mut reconnected, json!({"type":"response.create","model":"codex/gpt-test","input":[],"stream_id":"lane-3","prompt_cache_key":"conversation-cache","previous_response_id":"response-fixture"})).await;
    assert_eq!(next(&mut reconnected).await["type"], "response.completed");
    drop(reconnected);
    fixture.closed(2).await;
    let received = fixture.received();
    assert_eq!(received.len(), 5);
    assert_eq!(
        received[2].body, cancel,
        "cancel payload must pass through unchanged"
    );
    assert_eq!(received[0].body["previous_response_id"], Value::Null);
    assert_eq!(received[1].body["previous_response_id"], "response-fixture");
    assert_eq!(received[4].body["previous_response_id"], "response-fixture");
    for request in received
        .iter()
        .filter(|r| r.body["type"] == "response.create")
    {
        assert_eq!(request.body["model"], "gpt-test");
        assert_eq!(request.body["prompt_cache_key"], "conversation-cache");
        assert_private_headers(&request.headers, "conversation-ws");
    }
    assert_eq!(received[0].socket, received[1].socket);
    assert_ne!(received[0].socket, received[4].socket);
    assert_eq!(
        received[0].headers["authorization"], received[4].headers["authorization"],
        "reconnect must retain the subscription"
    );
    assert_eq!(
        received[0].headers["chatgpt-account-id"],
        received[4].headers["chatgpt-account-id"]
    );
    let calls = fixture.calls(4).await;
    assert_eq!(calls.len(), 4);
    assert!(
        calls
            .iter()
            .all(|call| call.cache_read_tokens == 80 && call.error.is_none())
    );
    assert_eq!(fixture.state.ledger.in_flight.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn unicode_sse_and_trailing_usage_survive_native_translation_and_collection() {
    let fixture = Fixture::new(&[("codex", "sse")], Strategy::FillFirst).await;
    *fixture.mock.trailing_frame.lock().unwrap() = true;
    let requests = [
        (
            "/v1/responses",
            json!({"model":"codex/gpt-test","input":"hello","stream":true}),
        ),
        (
            "/v1/messages",
            json!({"model":"codex/gpt-test","messages":[{"role":"user","content":"hello"}],"max_tokens":20,"stream":true}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"codex/gpt-test","messages":[{"role":"user","content":"hello"}],"stream":true}),
        ),
        (
            "/v1/responses",
            json!({"model":"codex/gpt-test","input":"hello","stream":false}),
        ),
    ];
    for (path, body) in requests {
        let response = fixture.post(path, "conversation-sse", body).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let content = response.text().await.unwrap();
        assert!(
            content.contains(TEXT),
            "{path}: Unicode text was lost: {content}"
        );
        assert!(
            !content.contains("event: error"),
            "{path}: complete upstream stream was treated as interrupted: {content}"
        );
        if path == "/v1/messages" {
            assert!(content.contains("cache_read_input_tokens\":80"));
        }
    }
    let calls = fixture.calls(4).await;
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|call| call.input_tokens == 100
        && call.output_tokens == 10
        && call.cache_read_tokens == 80
        && call.error.is_none()));
    for request in fixture.received() {
        assert_private_headers(&request.headers, "conversation-sse");
        assert_eq!(request.headers["session-id"], "conversation-sse");
        assert_eq!(request.headers["x-opencode-session"], "conversation-sse");
        assert!(
            request.body["prompt_cache_key"]
                .as_str()
                .is_some_and(|key| !key.is_empty())
        );
    }
}

#[tokio::test]
async fn codex_stream_without_content_type_is_still_read_as_events() {
    let fixture = Fixture::new(&[("codex", "alpha")], Strategy::FillFirst).await;
    fixture
        .mock
        .omit_content_type
        .store(true, Ordering::Relaxed);
    for (path, body) in [
        (
            "/v1/responses",
            json!({"model":"gpt-test","input":"hello","stream":false}),
        ),
        (
            "/v1/responses",
            json!({"model":"gpt-test","input":"hello","stream":true}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"gpt-test","messages":[{"role":"user","content":"hello"}],"stream":false}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"gpt-test","messages":[{"role":"user","content":"hello"}],"stream":true}),
        ),
    ] {
        let response = fixture
            .post(path, "conversation-untyped", body.clone())
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{path} {body}");
        let text = response.text().await.unwrap();
        assert!(text.contains(TEXT), "{path} {body}: {text}");
    }
}

#[tokio::test]
async fn the_request_timeout_bounds_upstream_silence_not_the_whole_stream() {
    let fixture = Fixture::new(&[("codex", "alpha")], Strategy::FillFirst).await;
    fixture
        .state
        .config
        .write()
        .await
        .settings
        .request_timeout_seconds = 1;
    // Three 400 ms gaps: the answer outlasts the timeout, no silence does.
    fixture.mock.chunk_pause_ms.store(400, Ordering::Relaxed);
    let requests = [
        (
            "/v1/responses",
            json!({"model":"gpt-test","input":"hello","stream":true}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"gpt-test","messages":[{"role":"user","content":"hello"}],"stream":true}),
        ),
        (
            "/v1/responses",
            json!({"model":"gpt-test","input":"hello","stream":false}),
        ),
    ];
    for (path, body) in requests {
        let started = std::time::Instant::now();
        let response = fixture.post(path, "conversation-slow", body.clone()).await;
        assert_eq!(response.status(), StatusCode::OK, "{path} {body}");
        let text = response.text().await.unwrap();
        assert!(started.elapsed() > Duration::from_secs(1), "{path} {body}");
        assert!(text.contains(TEXT), "{path} {body}: {text}");
        assert!(!text.contains("event: error"), "{path} {body}: {text}");
    }
    let calls = fixture.calls(3).await;
    assert!(calls.iter().all(|call| call.error.is_none()), "{calls:?}");
}

#[tokio::test]
async fn an_upstream_silent_past_the_request_timeout_ends_as_stalled() {
    let fixture = Fixture::new(&[("codex", "alpha")], Strategy::FillFirst).await;
    fixture
        .state
        .config
        .write()
        .await
        .settings
        .request_timeout_seconds = 1;
    fixture.mock.chunk_pause_ms.store(2_500, Ordering::Relaxed);
    let streamed = fixture
        .post(
            "/v1/responses",
            "conversation-stalled",
            json!({"model":"gpt-test","input":"hello","stream":true}),
        )
        .await;
    assert_eq!(streamed.status(), StatusCode::OK);
    let text = streamed.text().await.unwrap();
    assert!(text.contains("event: error"), "{text}");
    assert!(text.contains("upstream stream stalled"), "{text}");
    let collected = fixture
        .post(
            "/v1/responses",
            "conversation-stalled",
            json!({"model":"gpt-test","input":"hello","stream":false}),
        )
        .await;
    assert_eq!(collected.status(), StatusCode::BAD_GATEWAY);
    assert!(
        collected
            .text()
            .await
            .unwrap()
            .contains("upstream stream stalled")
    );
    let calls = fixture.calls(2).await;
    assert!(
        calls
            .iter()
            .all(|call| call.error.as_deref() == Some("upstream stream stalled")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn codex_items_fill_a_completion_that_arrives_with_an_empty_output() {
    let fixture = Fixture::new(&[("codex", "alpha")], Strategy::FillFirst).await;
    fixture
        .mock
        .items_before_empty_completion
        .store(true, Ordering::Relaxed);
    for (path, body) in [
        (
            "/v1/responses",
            json!({"model":"gpt-test","input":"hello","stream":false}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"gpt-test","messages":[{"role":"user","content":"hello"}],"stream":false}),
        ),
    ] {
        let response = fixture.post(path, "conversation-items", body.clone()).await;
        assert_eq!(response.status(), StatusCode::OK, "{path} {body}");
        let text = response.text().await.unwrap();
        assert!(text.contains(TEXT), "{path} {body}: {text}");
    }
    // Codex's backend refuses a plain text input.
    for request in fixture.received() {
        assert!(request.body["input"].is_array(), "{}", request.body);
    }
}

#[tokio::test]
async fn rate_or_auth_failure_rotates_inside_provider_then_pins_the_replacement_after_recovery() {
    for rejected_status in [429, 401, 403] {
        let fixture = Fixture::new(
            &[
                ("codex", "alpha"),
                ("codex", "beta"),
                ("openai_compatible", "other-provider"),
            ],
            Strategy::FillFirst,
        )
        .await;
        fixture
            .mock
            .rejected_status
            .store(rejected_status, Ordering::Relaxed);
        let first = fixture
            .accounts
            .iter()
            .filter(|(_, provider, _)| provider == "codex")
            .min_by_key(|(id, _, _)| id)
            .unwrap();
        *fixture.mock.rejected_token.lock().unwrap() = Some(format!("Bearer fixture-{}", first.2));
        let request = json!({"model":"gpt-test","input":"hello","prompt_cache_key":"stable-http-cache","stream":false});
        let response = fixture
            .post("/v1/responses", "conversation-http", request.clone())
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.unwrap()["usage"]["input_tokens_details"]["cached_tokens"],
            80
        );
        let received = fixture.received();
        assert_eq!(
            received.len(),
            2,
            "only one retry within the selected provider"
        );
        assert_ne!(
            received[0].headers["authorization"],
            received[1].headers["authorization"]
        );
        assert!(
            received
                .iter()
                .all(|r| r.headers["authorization"] != "Bearer fixture-other-provider")
        );
        // Restore the first account's eligibility. The next request must remain pinned,
        // rather than passing only because the original account is cooling down.
        *fixture.mock.rejected_token.lock().unwrap() = None;
        let imported = fixture
            .state
            .accounts
            .import(
                account("codex", &first.2, &fixture.upstream.url),
                Some(first.2.clone()),
            )
            .await
            .unwrap();
        assert_eq!(imported.id, first.0);
        assert_eq!(imported.status, "ready");
        let response = fixture
            .post("/v1/responses", "conversation-http", request)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.text().await.unwrap();
        let received = fixture.received();
        assert_eq!(received.len(), 3);
        assert_eq!(
            received[1].headers["authorization"],
            received[2].headers["authorization"]
        );
        for request in &received {
            assert_private_headers(&request.headers, "conversation-http");
            assert_eq!(request.body["prompt_cache_key"], "stable-http-cache");
        }
        let calls = fixture.calls(3).await;
        assert_eq!(
            calls
                .iter()
                .filter(|call| usize::from(call.status) == rejected_status)
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.cache_read_tokens == 80 && call.error.is_none())
                .count(),
            2
        );
    }
}

#[tokio::test]
async fn disabling_model_rejects_http_new_socket_and_next_turn_on_existing_socket() {
    let fixture = Fixture::new(&[("codex", "disabled")], Strategy::FillFirst).await;
    let mut socket = fixture.socket("conversation-disabled").await;
    send(
        &mut socket,
        json!({"type":"response.create","model":"codex/gpt-test","input":[]}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    fixture
        .state
        .config
        .write()
        .await
        .disabled_models
        .insert("codex/gpt-test".into());
    fixture.state.refresh_models().await;
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
            .all(|model| model["id"] != "codex/gpt-test")
    );
    let response = fixture
        .post(
            "/v1/responses",
            "conversation-disabled",
            json!({"model":"codex/gpt-test","input":"blocked"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let mut fresh = fixture.socket("fresh-disabled").await;
    send(
        &mut fresh,
        json!({"type":"response.create","model":"codex/gpt-test","input":[]}),
    )
    .await;
    assert_eq!(next(&mut fresh).await["type"], "error");
    let _ = fresh.close(None).await;
    send(
        &mut socket,
        json!({"type":"response.create","model":"gpt-test","input":[]}),
    )
    .await;
    assert_eq!(
        next(&mut socket).await["type"],
        "error",
        "disabled model must not remain usable on an established socket"
    );
    let _ = socket.close(None).await;
    fixture.closed(1).await;
    assert_eq!(
        fixture.received().len(),
        1,
        "disabled inference must never reach upstream"
    );
}

async fn terminal(socket: &mut ClientSocket) -> Value {
    loop {
        let event = next(socket).await;
        if matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.incomplete" | "error")
        ) {
            return event;
        }
    }
}

#[tokio::test]
async fn native_websocket_failover_before_output_preserves_cache_and_pins_replacement() {
    for failure in [
        json!({"type":"error","status":429,"error":{"type":"usage_limit_reached","resets_in_seconds":3600}}),
        json!({"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","resets_at":time::OffsetDateTime::now_utc().unix_timestamp()+3600}}}),
        json!({"type":"error","status":401,"error":{"code":"invalid_api_key"}}),
    ] {
        let fixture = Fixture::new(
            &[
                ("codex", "alpha"),
                ("codex", "beta"),
                ("openai_compatible", "other-provider"),
            ],
            Strategy::FillFirst,
        )
        .await;
        let first = fixture
            .accounts
            .iter()
            .filter(|(_, provider, _)| provider == "codex")
            .min_by_key(|(id, _, _)| id)
            .unwrap();
        *fixture.mock.rejected_token.lock().unwrap() = Some(format!("Bearer fixture-{}", first.2));
        *fixture.mock.native_failure.lock().unwrap() = Some(failure.clone());
        let mut socket = fixture.socket("native-failover").await;
        let request = json!({"type":"response.create","model":"codex/gpt-test","input":[{"role":"user","content":"full request"}],"stream_id":"lane","prompt_cache_key":"stable-native-cache"});
        send(&mut socket, request.clone()).await;
        assert_eq!(
            next(&mut socket).await["type"],
            "response.completed",
            "an unstarted request must retry within its provider: {failure}"
        );
        let received = fixture.received();
        assert_eq!(received.len(), 2);
        assert_ne!(
            received[0].headers["authorization"],
            received[1].headers["authorization"]
        );
        assert!(
            received
                .iter()
                .all(|r| r.headers["authorization"] != "Bearer fixture-other-provider")
        );
        assert_eq!(
            fixture
                .state
                .accounts
                .list()
                .await
                .iter()
                .find(|a| a.id == first.0)
                .unwrap()
                .status,
            "cooldown"
        );
        assert_eq!(
            fixture
                .state
                .accounts
                .eligible(
                    douane::subscriptions::SubscriptionProvider::Codex,
                    Some("gpt-test")
                )
                .await
                .len(),
            1
        );
        let mut continuation = request.clone();
        continuation["previous_response_id"] = json!("response-fixture");
        send(&mut socket, continuation.clone()).await;
        assert_eq!(next(&mut socket).await["type"], "response.completed");
        assert_eq!(
            fixture.received()[1].socket,
            fixture.received()[2].socket,
            "healthy replacement must reuse its socket"
        );
        let _ = socket.close(None).await;
        fixture.closed(2).await;
        // Recovery of the original account must not steal an established conversation.
        fixture
            .state
            .accounts
            .import(
                account("codex", &first.2, &fixture.upstream.url),
                Some(first.2.clone()),
            )
            .await
            .unwrap();
        let mut fresh = fixture.socket("native-failover").await;
        send(&mut fresh, continuation).await;
        assert_eq!(next(&mut fresh).await["type"], "response.completed");
        let received = fixture.received();
        assert_eq!(received.len(), 4);
        assert_eq!(
            received[1].headers["authorization"],
            received[3].headers["authorization"]
        );
        assert_eq!(received[3].body["previous_response_id"], "response-fixture");
        for request in &received {
            assert_private_headers(&request.headers, "native-failover");
            assert_eq!(request.body["prompt_cache_key"], "stable-native-cache");
            assert_eq!(
                request.body["input"],
                json!([{"role":"user","content":"full request"}])
            );
        }
        let _ = fresh.close(None).await;
        let calls = fixture.calls(4).await;
        assert_eq!(calls.iter().filter(|c| c.error.is_some()).count(), 1);
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.cache_read_tokens == 80 && c.error.is_none())
                .count(),
            3
        );
        assert_eq!(fixture.state.ledger.in_flight.load(Ordering::Relaxed), 0);
    }
}

async fn closed_socket(socket: &mut ClientSocket) {
    timeout(WAIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Ping(data))) => {
                    let _ = socket.send(Message::Pong(data)).await;
                }
                event => panic!("failed socket must close without replaying events: {event:?}"),
            }
        }
    })
    .await
    .expect("failed subscription socket must close");
}

#[tokio::test]
async fn native_websocket_partial_quota_failure_never_replays_and_reconnect_needs_full_input() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    let first = fixture.accounts.iter().min_by_key(|(id, _, _)| id).unwrap();
    *fixture.mock.rejected_token.lock().unwrap() = Some(format!("Bearer fixture-{}", first.2));
    *fixture.mock.native_failure.lock().unwrap() = Some(
        json!({"type":"response.failed","response":{"error":{"code":"usage_limit_reached","resets_in_seconds":3600}}}),
    );
    fixture.mock.native_partial.store(true, Ordering::Relaxed);
    let mut socket = fixture.socket("native-partial").await;
    send(&mut socket,json!({"type":"response.create","model":"codex/gpt-test","input":"first","stream_id":"partial","prompt_cache_key":"partial-cache"})).await;
    assert_eq!(next(&mut socket).await["type"], "response.created");
    assert_eq!(next(&mut socket).await["delta"], "already emitted");
    assert_eq!(next(&mut socket).await["type"], "response.failed");
    closed_socket(&mut socket).await;
    fixture.closed(1).await;
    assert_eq!(
        fixture.received().len(),
        1,
        "partial output must never replay on another subscription"
    );
    let mut incremental = fixture.socket("native-partial").await;
    send(&mut incremental,json!({"type":"response.create","model":"codex/gpt-test","input":"delta only","previous_response_id":"failed-response","prompt_cache_key":"partial-cache"})).await;
    assert_eq!(
        next(&mut incremental).await["error"]["code"],
        "previous_response_not_found"
    );
    closed_socket(&mut incremental).await;
    assert_eq!(
        fixture.received().len(),
        1,
        "an old account's response ID must never reach its replacement"
    );
    let mut full = fixture.socket("native-partial").await;
    let history = json!([{"role":"user","content":"first"},{"role":"assistant","content":"already emitted"},{"role":"user","content":"continue"}]);
    send(&mut full,json!({"type":"response.create","model":"codex/gpt-test","input":history,"previous_response_id":null,"prompt_cache_key":"partial-cache"})).await;
    assert_eq!(next(&mut full).await["type"], "response.completed");
    let received = fixture.received();
    assert_eq!(received.len(), 2);
    assert_ne!(
        received[0].headers["authorization"],
        received[1].headers["authorization"]
    );
    assert_eq!(received[1].body["input"], history);
    assert_eq!(received[1].body["previous_response_id"], Value::Null);
    assert_eq!(received[1].body["prompt_cache_key"], "partial-cache");
    let _ = full.close(None).await;
    let calls = fixture.calls(2).await;
    assert_eq!(calls.iter().filter(|c| c.error.is_some()).count(), 1);
    assert_eq!(fixture.state.ledger.in_flight.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn native_websocket_rechecks_cooldown_before_each_continuation() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    let mut socket = fixture.socket("native-external-cooldown").await;
    send(
        &mut socket,
        json!({"type":"response.create","model":"codex/gpt-test","input":[]}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    let token = fixture.received()[0].headers["authorization"]
        .to_str()
        .unwrap()
        .to_owned();
    let first = fixture
        .accounts
        .iter()
        .find(|(_, _, name)| token == format!("Bearer fixture-{name}"))
        .unwrap();
    fixture
        .state
        .accounts
        .cooldown(&first.0, 3600, "fixture external cooldown")
        .await
        .unwrap();
    send(&mut socket,json!({"type":"response.create","model":"gpt-test","input":"delta","previous_response_id":"response-fixture"})).await;
    assert_eq!(
        next(&mut socket).await["error"]["code"],
        "previous_response_not_found"
    );
    closed_socket(&mut socket).await;
    assert_eq!(
        fixture.received().len(),
        1,
        "cooldown must apply to persistent sockets too"
    );
}

#[tokio::test]
async fn native_websocket_rate_limit_on_the_only_account_leaves_the_next_request_to_upstream() {
    let fixture = Fixture::new(&[("codex", "alpha")], Strategy::FillFirst).await;
    *fixture.mock.rejected_token.lock().unwrap() = Some("Bearer fixture-alpha".into());
    *fixture.mock.native_failure.lock().unwrap() = Some(
        json!({"type":"error","status":429,"error":{"type":"rate_limit_exceeded","code":"rate_limit_exceeded","resets_in_seconds":400}}),
    );
    let mut limited = fixture.socket("native-rate-limit").await;
    send(
        &mut limited,
        json!({"type":"response.create","model":"codex/gpt-test","input":"first"}),
    )
    .await;
    assert_eq!(next(&mut limited).await["status"], 429);
    let _ = limited.close(None).await;
    *fixture.mock.native_failure.lock().unwrap() = None;
    // Another session during the 400 s rest: the gateway used to refuse it
    // with "no available subscription" while the upstream would serve it.
    let mut other = fixture.socket("native-rate-limit-other").await;
    send(
        &mut other,
        json!({"type":"response.create","model":"codex/gpt-test","input":"second"}),
    )
    .await;
    assert_eq!(next(&mut other).await["type"], "response.completed");
    let _ = other.close(None).await;
    let calls = fixture.calls(2).await;
    assert!(calls.iter().any(|call| {
        call.error
            .as_deref()
            .is_some_and(|error| error.ends_with("(rate_limit_exceeded)"))
    }));
    assert_eq!(fixture.received().len(), 2);
}

#[tokio::test]
async fn native_websocket_request_error_without_lane_releases_the_only_active_call() {
    let fixture = Fixture::new(&[("codex", "alpha")], Strategy::FillFirst).await;
    *fixture.mock.rejected_token.lock().unwrap() = Some("Bearer fixture-alpha".into());
    *fixture.mock.native_failure.lock().unwrap() =
        Some(json!({"type":"error","status":400,"error":{"code":"invalid_request_error"}}));
    fixture.mock.native_partial.store(true, Ordering::Relaxed);
    fixture.mock.omit_native_lane.store(true, Ordering::Relaxed);
    let mut socket = fixture.socket("native-request-error").await;
    send(&mut socket, json!({"type":"response.create","model":"codex/gpt-test","input":"invalid","stream_id":"named"})).await;
    assert_eq!(next(&mut socket).await["type"], "response.created");
    assert_eq!(
        next(&mut socket).await["type"],
        "response.output_text.delta"
    );
    assert_eq!(next(&mut socket).await["status"], 400);
    assert_eq!(
        fixture.state.ledger.in_flight.load(Ordering::Relaxed),
        0,
        "an unlabelled terminal must release the sole named call"
    );
    *fixture.mock.native_failure.lock().unwrap() = None;
    send(
        &mut socket,
        json!({"type":"response.create","model":"gpt-test","input":"valid","stream_id":"named"}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    let _ = socket.close(None).await;
    let calls = fixture.calls(2).await;
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.error.is_some() && call.cache_read_tokens == 0)
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.error.is_none()
                && call.cache_read_tokens == 80
                && call.input_tokens == 100)
            .count(),
        1
    );
    assert_eq!(fixture.mock.connections.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn native_websocket_namespaced_model_stays_on_the_same_subscription_and_respects_disable() {
    let fixture = Fixture::new(&[("openai_compatible", "namespaced")], Strategy::FillFirst).await;
    let mut credential = account("openai_compatible", "namespaced", &fixture.upstream.url);
    credential["models"] = json!(["vendor/model"]);
    fixture
        .state
        .accounts
        .import(credential, Some("namespaced".into()))
        .await
        .unwrap();
    fixture.state.refresh_models().await;
    let mut socket = fixture.socket("native-namespaced-model").await;
    send(
        &mut socket,
        json!({"type":"response.create","model":"openai-compatible/vendor/model","input":"first"}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    send(
        &mut socket,
        json!({"type":"response.create","model":"vendor/model","input":"second"}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    assert_eq!(fixture.mock.connections.load(Ordering::Relaxed), 1);
    assert!(
        fixture
            .received()
            .iter()
            .all(|request| request.body["model"] == "vendor/model")
    );
    fixture
        .state
        .config
        .write()
        .await
        .disabled_models
        .insert("openai_compatible/vendor/model".into());
    send(
        &mut socket,
        json!({"type":"response.create","model":"vendor/model","input":"disabled"}),
    )
    .await;
    assert_eq!(
        next(&mut socket).await["error"]["message"],
        "this model is disabled"
    );
    closed_socket(&mut socket).await;
    assert_eq!(fixture.received().len(), 2);
}

#[tokio::test]
async fn native_websocket_account_error_without_lane_tracks_the_only_active_named_response() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    let first = fixture.accounts.iter().min_by_key(|(id, _, _)| id).unwrap();
    let mut socket = fixture.socket("native-omitted-lane").await;
    send(
        &mut socket,
        json!({"type":"response.create","model":"codex/gpt-test","input":"first"}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    *fixture.mock.rejected_token.lock().unwrap() = Some(format!("Bearer fixture-{}", first.2));
    *fixture.mock.native_failure.lock().unwrap() =
        Some(json!({"type":"error","status":429,"error":{"code":"usage_limit_reached"}}));
    fixture.mock.omit_native_lane.store(true, Ordering::Relaxed);
    send(
        &mut socket,
        json!({"type":"response.create","model":"gpt-test","input":"second","stream_id":"named"}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    let _ = socket.close(None).await;
    fixture.closed(2).await;
    let calls = fixture.calls(3).await;
    assert_eq!(calls.iter().filter(|call| call.status == 429).count(), 1);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.status == 200 && call.error.is_none())
            .count(),
        2
    );
    assert_eq!(fixture.state.ledger.in_flight.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn native_websocket_replans_for_a_subscription_enabled_after_connection() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    let first = fixture.accounts.iter().min_by_key(|(id, _, _)| id).unwrap();
    let second = fixture
        .accounts
        .iter()
        .find(|(id, _, _)| id != &first.0)
        .unwrap();
    fixture
        .state
        .accounts
        .set_enabled(&second.0, false)
        .await
        .unwrap();
    let mut socket = fixture.socket("native-added-account").await;
    send(
        &mut socket,
        json!({"type":"response.create","model":"codex/gpt-test","input":"first full request"}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    fixture
        .state
        .accounts
        .set_enabled(&second.0, true)
        .await
        .unwrap();
    *fixture.mock.rejected_token.lock().unwrap() = Some(format!("Bearer fixture-{}", first.2));
    *fixture.mock.native_failure.lock().unwrap() = Some(
        json!({"type":"error","status":429,"error":{"code":"usage_limit_reached","resets_in_seconds":3600}}),
    );
    send(
        &mut socket,
        json!({"type":"response.create","model":"gpt-test","input":"second full request"}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    let received = fixture.received();
    assert_eq!(received.len(), 3);
    assert_eq!(
        received[2].headers["authorization"],
        format!("Bearer fixture-{}", second.2)
    );
    assert_eq!(received[1].body, received[2].body);
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn native_websocket_unknown_previous_response_requires_full_input_without_dialing() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    let mut socket = fixture.socket("native-unknown-origin").await;
    send(&mut socket, json!({"type":"response.create","model":"codex/gpt-test","input":"delta only","previous_response_id":"from-another-runtime","stream_id":"missing"})).await;
    let error = next(&mut socket).await;
    assert_eq!(error["error"]["code"], "previous_response_not_found");
    assert_eq!(error["stream_id"], "missing");
    closed_socket(&mut socket).await;
    assert_eq!(fixture.mock.connections.load(Ordering::Relaxed), 0);
    assert!(fixture.received().is_empty());
}

#[tokio::test]
async fn native_websocket_old_response_never_reaches_a_pinned_replacement() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    fixture
        .mock
        .unique_native_ids
        .store(true, Ordering::Relaxed);
    let first = fixture.accounts.iter().min_by_key(|(id, _, _)| id).unwrap();
    let mut socket = fixture.socket("native-old-response").await;
    send(
        &mut socket,
        json!({"type":"response.create","model":"codex/gpt-test","input":"initial"}),
    )
    .await;
    let old = next(&mut socket).await["response"]["id"].clone();
    *fixture.mock.rejected_token.lock().unwrap() = Some(format!("Bearer fixture-{}", first.2));
    *fixture.mock.native_failure.lock().unwrap() = Some(
        json!({"type":"error","status":429,"error":{"code":"usage_limit_reached","resets_in_seconds":3600}}),
    );
    send(
        &mut socket,
        json!({"type":"response.create","model":"gpt-test","input":"full history"}),
    )
    .await;
    let replacement = next(&mut socket).await["response"]["id"].clone();
    assert_ne!(old, replacement);
    send(&mut socket, json!({"type":"response.create","model":"gpt-test","input":"delta","previous_response_id":old})).await;
    assert_eq!(
        next(&mut socket).await["error"]["code"],
        "previous_response_not_found"
    );
    closed_socket(&mut socket).await;
    assert_eq!(fixture.received().len(), 3);
    fixture
        .state
        .accounts
        .import(
            account("codex", &first.2, &fixture.upstream.url),
            Some(first.2.clone()),
        )
        .await
        .unwrap();
    let mut reconnected = fixture.socket("native-old-response").await;
    send(&mut reconnected, json!({"type":"response.create","model":"codex/gpt-test","input":"delta","previous_response_id":old})).await;
    assert_eq!(
        next(&mut reconnected).await["error"]["code"],
        "previous_response_not_found"
    );
    closed_socket(&mut reconnected).await;
    assert_eq!(fixture.mock.connections.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn native_websocket_known_response_owner_is_used_even_behind_another_candidate() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    fixture
        .mock
        .unique_native_ids
        .store(true, Ordering::Relaxed);
    let first = fixture.accounts.iter().min_by_key(|(id, _, _)| id).unwrap();
    let second = fixture
        .accounts
        .iter()
        .find(|(id, _, _)| id != &first.0)
        .unwrap();
    fixture
        .state
        .accounts
        .set_enabled(&first.0, false)
        .await
        .unwrap();
    let mut original = fixture.socket("native-known-owner-original").await;
    send(
        &mut original,
        json!({"type":"response.create","model":"codex/gpt-test","input":"initial"}),
    )
    .await;
    let previous = next(&mut original).await["response"]["id"].clone();
    let _ = original.close(None).await;
    fixture.closed(1).await;
    fixture
        .state
        .accounts
        .set_enabled(&first.0, true)
        .await
        .unwrap();
    // A new conversation pin is unknown, but the opaque response's subscription is known.
    let mut reconnected = fixture.socket("native-known-owner-new-session").await;
    send(&mut reconnected, json!({"type":"response.create","model":"codex/gpt-test","input":"delta","previous_response_id":previous})).await;
    assert_eq!(next(&mut reconnected).await["type"], "response.completed");
    let _ = reconnected.close(None).await;
    fixture.closed(2).await;
    assert_eq!(fixture.mock.connections.load(Ordering::Relaxed), 2);
    assert!(
        fixture
            .received()
            .iter()
            .all(|r| r.headers["authorization"] == format!("Bearer fixture-{}", second.2))
    );
}

#[tokio::test]
async fn native_websocket_incremental_quota_failure_requests_recognized_reconstruction() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    let first = fixture.accounts.iter().min_by_key(|(id, _, _)| id).unwrap();
    let mut socket = fixture.socket("native-incremental-quota").await;
    send(&mut socket, json!({"type":"response.create","model":"codex/gpt-test","input":"initial","prompt_cache_key":"incremental-cache"})).await;
    let previous = next(&mut socket).await["response"]["id"].clone();
    *fixture.mock.rejected_token.lock().unwrap() = Some(format!("Bearer fixture-{}", first.2));
    *fixture.mock.native_failure.lock().unwrap() = Some(
        json!({"type":"error","status":429,"error":{"code":"usage_limit_reached","resets_in_seconds":3600}}),
    );
    send(&mut socket, json!({"type":"response.create","model":"gpt-test","input":"delta","previous_response_id":previous})).await;
    assert_eq!(
        next(&mut socket).await["error"]["code"],
        "previous_response_not_found"
    );
    closed_socket(&mut socket).await;
    assert_eq!(
        fixture.received().len(),
        2,
        "incremental input must never retry against another account"
    );
    let mut full = fixture.socket("native-incremental-quota").await;
    let history = json!([{"role":"user","content":"initial"},{"role":"assistant","content":TEXT},{"role":"user","content":"delta"}]);
    send(&mut full, json!({"type":"response.create","model":"codex/gpt-test","input":history,"previous_response_id":null,"prompt_cache_key":"incremental-cache"})).await;
    assert_eq!(next(&mut full).await["type"], "response.completed");
    let received = fixture.received();
    assert_ne!(
        received[1].headers["authorization"],
        received[2].headers["authorization"]
    );
    assert_eq!(received[2].body["input"], history);
    assert_eq!(received[2].body["prompt_cache_key"], "incremental-cache");
    let _ = full.close(None).await;
}

#[tokio::test]
async fn native_websocket_concurrent_lanes_fail_explicitly_without_replay() {
    let fixture = Fixture::new(
        &[("codex", "alpha"), ("codex", "beta")],
        Strategy::FillFirst,
    )
    .await;
    let first = fixture.accounts.iter().min_by_key(|(id, _, _)| id).unwrap();
    *fixture.mock.rejected_token.lock().unwrap() = Some(format!("Bearer fixture-{}", first.2));
    *fixture.mock.native_failure.lock().unwrap() =
        Some(json!({"type":"error","status":429,"error":{"code":"usage_limit_reached"}}));
    fixture.mock.hold_native.store(true, Ordering::Relaxed);
    let mut socket = fixture.socket("native-concurrent-quota").await;
    for lane in ["one", "two"] {
        send(&mut socket, json!({"type":"response.create","model":"codex/gpt-test","input":lane,"stream_id":lane})).await;
    }
    timeout(WAIT, async {
        while fixture.state.ledger.in_flight.load(Ordering::Relaxed) != 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    fixture.mock.native_release.notify_one();
    assert_eq!(
        next(&mut socket).await["error"]["code"],
        "usage_limit_reached"
    );
    closed_socket(&mut socket).await;
    fixture.closed(1).await;
    assert_eq!(
        fixture.mock.connections.load(Ordering::Relaxed),
        1,
        "concurrent lanes cannot be silently replayed on a replacement"
    );
    let calls = fixture.calls(2).await;
    assert!(calls.iter().all(|call| call.error.is_some()));
    assert_eq!(fixture.state.ledger.in_flight.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn native_websocket_heartbeat_expires_silent_downstream_with_healthy_upstream() {
    // Real time on a short beat: a paused clock left the loopback sockets
    // unpolled on Windows runners and timed the test out.
    let fixture = Fixture::new(&[("codex", "alpha")], Strategy::FillFirst).await;
    fixture
        .state
        .socket_heartbeat_ms
        .store(200, Ordering::Relaxed);
    let mut socket = fixture.socket("native-downstream-heartbeat").await;
    send(
        &mut socket,
        json!({"type":"response.create","model":"codex/gpt-test","input":[]}),
    )
    .await;
    assert_eq!(next(&mut socket).await["type"], "response.completed");
    let MaybeTlsStream::Plain(stream) = socket.get_mut() else {
        panic!("local fixture must use a plain socket");
    };
    // Reading via tungstenite would automatically send queued pongs after the close.
    // Inspect raw control frames to model a lost phone that never acknowledges pings.
    use tokio::io::AsyncReadExt;
    let mut pings = 0;
    timeout(WAIT, async {
        loop {
            let mut header = [0; 2];
            stream.read_exact(&mut header).await.unwrap();
            assert_eq!(header[1] & 0x80, 0);
            assert!(header[1] <= 125);
            let mut payload = vec![0; header[1] as usize];
            stream.read_exact(&mut payload).await.unwrap();
            match header[0] & 0x0f {
                9 => {
                    pings += 1;
                }
                8 => {
                    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1013);
                    break;
                }
                opcode => panic!("healthy upstream must not time out first: opcode {opcode}"),
            }
        }
    })
    .await
    .unwrap();
    fixture.closed(1).await;
    assert!(
        pings >= 2,
        "the silent peer was dropped after {pings} pings"
    );
    assert_eq!(
        fixture.mock.native_pings.load(Ordering::Relaxed),
        pings,
        "both peers must be pinged on the same heartbeat cadence"
    );
}

#[tokio::test]
async fn grok_cli_model_header_reaches_the_provider_with_its_own_id() {
    let fixture = Fixture::new(&[("xai", "grok-cli")], Strategy::FillFirst).await;
    let response = fixture
        .client
        .post(format!("{}/v1/chat/completions", fixture.gateway.url))
        .header("x-grok-model-override", "xai/gpt-test")
        .json(&json!({"model":"xai/gpt-test","messages":[{"role":"user","content":"hello"}],"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let received = fixture.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body["model"], "gpt-test");
    assert_eq!(received[0].headers["x-grok-model-override"], "gpt-test");
}

#[tokio::test]
async fn http_subscription_websocket_streams_history_cancellation_and_warmup() {
    let fixture = Fixture::new(&[("xai", "compatible")], Strategy::FillFirst).await;
    let mut socket = fixture.socket("conversation-compatible").await;
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":[],"generate":false,"stream_id":"warm","prompt_cache_key":"compatible-cache"})).await;
    let warmup = terminal(&mut socket).await;
    assert_eq!(warmup["type"], "response.completed");
    assert!(
        fixture.received().is_empty(),
        "warmup must not consume inference quota"
    );
    let previous = warmup["response"]["id"].clone();
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":"alpha","previous_response_id":previous,"stream_id":"first"})).await;
    let first = terminal(&mut socket).await;
    assert_eq!(first["type"], "response.completed");
    assert_eq!(first["stream_id"], "first");
    assert_eq!(
        first["response"]["usage"]["input_tokens_details"]["cached_tokens"],
        80
    );
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":"beta","previous_response_id":first["response"]["id"],"stream_id":"second"})).await;
    assert_eq!(terminal(&mut socket).await["stream_id"], "second");
    let received = fixture.received();
    assert_eq!(received.len(), 2);
    assert_eq!(
        fixture.mock.connections.load(Ordering::Relaxed),
        0,
        "HTTP subscriptions must use the streaming HTTP bridge"
    );
    assert_eq!(
        received[1].body["messages"][0]["content"][0]["text"],
        "alpha"
    );
    assert_eq!(received[1].body["messages"][1]["content"][0]["text"], TEXT);
    assert_eq!(
        received[1].body["messages"][2]["content"][0]["text"],
        "beta"
    );
    for request in received {
        assert_eq!(request.body["prompt_cache_key"], "compatible-cache");
        assert_eq!(request.headers["session_id"], "conversation-compatible");
        assert_eq!(
            request.headers["authorization"],
            "Bearer fixture-compatible"
        );
    }
    let mut fresh = fixture.socket("conversation-compatible").await;
    send(&mut fresh, json!({"type":"response.create","model":"xai/gpt-test","input":"reconnect","previous_response_id":first["response"]["id"]})).await;
    assert_eq!(
        terminal(&mut fresh).await["error"]["code"],
        "previous_response_not_found"
    );
    let _ = fresh.close(None).await;
    for _ in 0..9 {
        send(
            &mut socket,
            json!({"type":"response.create","model":"xai/gpt-test","input":[],"generate":false}),
        )
        .await;
        assert_eq!(terminal(&mut socket).await["type"], "response.completed");
    }
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":"evicted","previous_response_id":first["response"]["id"]})).await;
    assert_eq!(
        terminal(&mut socket).await["error"]["code"],
        "previous_response_not_found",
        "history must evict responses beyond its 8-entry bound"
    );
    assert_eq!(fixture.received().len(), 2);
    fixture.mock.hold_http.store(true, Ordering::Relaxed);
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":"cancel","stream_id":"cancel-lane"})).await;
    let created = next(&mut socket).await;
    assert_eq!(created["type"], "response.created");
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":"queued","stream_id":"other-lane"})).await;
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":"remove queued","stream_id":"cancel-lane"})).await;
    fixture.mock.hold_http.store(false, Ordering::Relaxed);
    send(&mut socket, json!({"type":"response.cancel","response_id":created["response"]["id"],"stream_id":"cancel-lane"})).await;
    let cancelled = terminal(&mut socket).await;
    assert_eq!(cancelled["type"], "response.incomplete");
    assert_eq!(cancelled["stream_id"], "cancel-lane");
    let queued = terminal(&mut socket).await;
    assert_eq!(
        queued["stream_id"], "other-lane",
        "cancelling a lane must preserve another lane's pending response"
    );
    assert_eq!(queued["type"], "response.completed");
    timeout(WAIT, async {
        while fixture.mock.closed_http.load(Ordering::Relaxed) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":"unknown","previous_response_id":"unknown-response","stream_id":"missing"})).await;
    let missing = terminal(&mut socket).await;
    assert_eq!(missing["error"]["code"], "previous_response_not_found");
    assert_eq!(missing["stream_id"], "missing");
    fixture.mock.hold_http.store(true, Ordering::Relaxed);
    send(&mut socket, json!({"type":"response.create","model":"xai/gpt-test","input":"disconnect","stream_id":"disconnect"})).await;
    assert_eq!(next(&mut socket).await["type"], "response.created");
    drop(socket);
    timeout(WAIT, async {
        while fixture.mock.closed_http.load(Ordering::Relaxed) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let calls = fixture.calls(5).await;
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.error.is_none() && call.cache_read_tokens == 80)
            .count(),
        3
    );
    assert_eq!(fixture.state.ledger.in_flight.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn native_answers_keep_the_provider_limits_and_refusals_without_blaming_the_account() {
    let fixture = Fixture::new(&[("codex", "alpha")], Strategy::FillFirst).await;
    let request = json!({"model":"gpt-test","input":"hello","stream":false});

    fixture.mock.limit_headers.store(true, Ordering::Relaxed);
    let answer = fixture
        .post("/v1/responses", "limits", request.clone())
        .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(answer.headers()["x-codex-primary-used-percent"], "17");
    assert!(answer.headers().get("set-cookie").is_none());

    let refusal = json!({"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"prompt is too long"}});
    *fixture.mock.refusal.lock().unwrap() = Some((400, refusal.clone()));
    let refused = fixture.post("/v1/responses", "refused", request).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(refused.headers()["x-codex-primary-used-percent"], "42");
    assert_eq!(refused.headers()["request-id"], "fixture-request");
    assert!(refused.headers().get("set-cookie").is_none());
    assert_eq!(refused.json::<Value>().await.unwrap(), refusal);
    let account = fixture
        .state
        .accounts
        .list()
        .await
        .into_iter()
        .next()
        .unwrap();
    assert!(
        account.last_error.is_none(),
        "a refused request leaves the account healthy"
    );
}

#[tokio::test]
async fn claude_token_counting_reaches_the_provider() {
    let fixture = Fixture::new(&[("claude", "alpha")], Strategy::FillFirst).await;
    let counted = fixture
        .post(
            "/v1/messages/count_tokens",
            "count",
            json!({"model":"gpt-test","messages":[{"role":"user","content":"hello"}]}),
        )
        .await;
    assert_eq!(counted.status(), StatusCode::OK);
    assert_eq!(
        counted.json::<Value>().await.unwrap(),
        json!({"input_tokens":12})
    );
}
