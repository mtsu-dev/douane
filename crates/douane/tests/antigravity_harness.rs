//! Synthetic native upstreams exercise complete signed tool history through the public API.
use axum::{Json, Router, body::Body, extract::State, response::IntoResponse, routing::post};
use douane::{settings::Config, state::AppState, translate::Frames};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Default)]
struct Mock {
    received: Mutex<Vec<Value>>,
    replies: Mutex<VecDeque<Value>>,
    force_stream: AtomicBool,
    empty_stream_sentinels: AtomicBool,
    refusal_status: AtomicU16,
}

struct Fixture {
    state: Arc<AppState>,
    mock: Arc<Mock>,
    url: String,
    client: reqwest::Client,
    tasks: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn serve(router: Router) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }),
    )
}

async fn native(
    State(mock): State<Arc<Mock>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if mock.force_stream.load(Ordering::Relaxed) {
        return native_stream(State(mock), Json(body)).await;
    }
    mock.received.lock().unwrap().push(body);
    if let Ok(status) =
        axum::http::StatusCode::from_u16(mock.refusal_status.load(Ordering::Relaxed))
    {
        return (
            status,
            [("retry-after", "7")],
            Json(json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"synthetic limit"}})),
        )
            .into_response();
    }
    Json(
        mock.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| reply(3)),
    )
    .into_response()
}

async fn native_stream(
    State(mock): State<Arc<Mock>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    mock.received.lock().unwrap().push(body);
    if let Ok(status) =
        axum::http::StatusCode::from_u16(mock.refusal_status.load(Ordering::Relaxed))
    {
        return (
            status,
            [("retry-after", "7")],
            Json(json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"synthetic limit"}})),
        )
            .into_response();
    }
    let reply = mock
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| reply(3));
    let parts = reply["response"]["candidates"][0]["content"]["parts"]
        .as_array()
        .unwrap();
    let mut data = String::new();
    let sentinels = mock.empty_stream_sentinels.load(Ordering::Relaxed);
    if sentinels {
        data.push_str(&format!(
            "data: {}\n\n",
            json!({"response":{"candidates":[{"content":{"parts":[{"text":""}]}}]}})
        ));
    }
    for part in parts {
        let event =
            json!({"response":{"candidates":[{"content":{"role":"model","parts":[part]}}]}});
        data.push_str(&format!("data: {event}\n\n"));
    }
    if let Some(reason) = reply.pointer("/response/candidates/0/finishReason") {
        let mut event = json!({"response":{"candidates":[{"finishReason":reason}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3}}});
        if sentinels {
            event["response"]["candidates"][0]["content"] = json!({"parts":[{"text":""}]});
        }
        data.push_str(&format!("data: {event}\n\n"));
    }
    if let Some(error) = reply.get("tail_error") {
        let event = json!({"response":{"error":error}});
        data.push_str(&format!("data: {event}\n\n"));
    }
    ([("content-type", "text/event-stream")], Body::from(data)).into_response()
}

#[tokio::test]
async fn nested_native_stream_error_is_sanitized_and_never_records_a_turn() {
    let mut failed = reply(1);
    failed["response"]["candidates"][0]["content"]["parts"][3]["functionCall"]["id"] =
        json!("call_failed");
    failed["tail_error"] = json!({"message":"synthetic-private-account-detail","code":500});
    let fixture = Fixture::new("antigravity", vec![failed]).await;
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[], "antigravity"),
            Some("native-error"),
            true,
        )
        .await;
    assert_eq!(status, 200);
    assert!(body.contains("event: error"), "{body}");
    assert!(!body.contains("synthetic-private-account-detail"));
    // A whole upstream chunk can contain both the call and its later error;
    // no partial translated output from that chunk needs to reach the client.
    let id = "call_failed".to_owned();
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[id], "antigravity"),
            Some("native-error"),
            false,
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(fixture.mock.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn native_sse_can_be_collected_for_a_json_client_without_losing_signatures() {
    let fixture = Fixture::new("antigravity", vec![reply(1), reply(3)]).await;
    fixture.mock.force_stream.store(true, Ordering::Relaxed);
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[], "antigravity"),
            Some("collected"),
            false,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let id = call_id("chat/completions", &body, false);
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[id], "antigravity"),
            Some("collected"),
            false,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("DONE"));
    let received = fixture.mock.received.lock().unwrap();
    assert_eq!(
        received[1]["request"]["contents"][1]["parts"][3]["thoughtSignature"],
        "SYNTHETIC_CALL_1"
    );
}

impl Fixture {
    async fn new(provider: &str, replies: Vec<Value>) -> Self {
        let mock = Arc::new(Mock::default());
        mock.replies.lock().unwrap().extend(replies);
        let (upstream, up_task) = serve(
            Router::new()
                .route("/v1internal:generateContent", post(native))
                .route("/v1internal:streamGenerateContent", post(native_stream))
                // Full replay histories can exceed Axum's 2 MiB mock default.
                .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
                .with_state(mock.clone()),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = dir.path().join("auth");
        config.server.database = dir.path().join("calls.sqlite");
        let state = AppState::new(config, None).unwrap();
        let (_, key) = state
            .clients
            .create(douane::clients::ClientPatch {
                name: Some("tests".into()),
                ..Default::default()
            })
            .unwrap();
        state.accounts.import(json!({"type":provider,"access_token":"synthetic-access","base_url":upstream,"models":["synthetic-thinking","claude-synthetic"]}), None).await.unwrap();
        state.refresh_models().await;
        let (url, gate_task) = serve(douane::router(state.clone())).await;
        Self {
            state,
            mock,
            url,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
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
            tasks: vec![up_task, gate_task],
            _dir: dir,
        }
    }

    async fn post(
        &self,
        kind: &str,
        mut request: Value,
        session: Option<&str>,
        stream: bool,
    ) -> (u16, String) {
        request["stream"] = json!(stream);
        let mut builder = self.client.post(format!("{}/v1/{}", self.url, kind));
        // OpenCode 1.19 sends this header for a custom provider.
        if let Some(session) = session {
            builder = builder.header("x-opencode-session-id", session);
        }
        let response = builder.json(&request).send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }
}

#[tokio::test]
async fn changed_system_instructions_cannot_borrow_a_signed_turn() {
    let mut initial = reply(1);
    initial["response"]["candidates"][0]["content"]["parts"]
        .as_array_mut()
        .unwrap()
        .retain(|p| p.get("text").is_none() || p["thought"] == true);
    let fixture = Fixture::new("antigravity", vec![initial]).await;
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[], "antigravity"),
            Some("system-change"),
            false,
        )
        .await;
    assert_eq!(status, 200);
    let id = call_id("chat/completions", &body, false);
    let mut changed = request("chat/completions", &[id], "antigravity");
    changed["messages"][0]["content"] = json!("Different instructions.");
    changed["messages"][2]["content"] = Value::Null;
    let (status, body) = fixture
        .post("chat/completions", changed, Some("system-change"), false)
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(fixture.mock.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn responses_parallel_calls_replay_as_one_original_native_turn() {
    let first = json!({"response":{"candidates":[{"content":{"role":"model","parts":[
        {"thought":true,"text":"synthetic parallel"},
        {"functionCall":{"name":"same","args":{"n":1}},"thoughtSignature":"SYNTHETIC_A"},
        {"functionCall":{"name":"same","args":{"n":1}},"thoughtSignature":"SYNTHETIC_B"}
    ]},"finishReason":"STOP"}]}});
    let fixture = Fixture::new("antigravity", vec![first, reply(3)]).await;
    let (status, body) = fixture
        .post(
            "responses",
            request("responses", &[], "antigravity"),
            Some("parallel"),
            false,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    let calls = value["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["type"] == "function_call")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0]["call_id"], calls[1]["call_id"]);
    let mut follow = request("responses", &[], "antigravity");
    let input = follow["input"].as_array_mut().unwrap();
    input.extend(calls.iter().cloned());
    for call in &calls {
        input.push(json!({"type":"function_call_output","call_id":call["call_id"],"output":"synthetic parallel result"}));
    }
    let (status, body) = fixture
        .post("responses", follow, Some("parallel"), false)
        .await;
    assert_eq!(status, 200, "{body}");
    let received = fixture.mock.received.lock().unwrap();
    let parts = &received[1]["request"]["contents"][1]["parts"];
    assert_eq!(parts[1]["thoughtSignature"], "SYNTHETIC_A");
    assert_eq!(parts[2]["thoughtSignature"], "SYNTHETIC_B");
}

#[tokio::test]
async fn unspecified_stream_finish_does_not_manufacture_completion() {
    let mut incomplete = reply(3);
    incomplete["response"]["candidates"][0]["finishReason"] = json!("FINISH_REASON_UNSPECIFIED");
    let fixture = Fixture::new("antigravity", vec![incomplete]).await;
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[], "antigravity"),
            Some("unspecified"),
            true,
        )
        .await;
    assert_eq!(status, 200);
    assert!(body.contains("event: error"), "{body}");
    assert!(!body.contains("[DONE]"));
}

#[tokio::test]
async fn explicit_thinking_uses_the_models_advertised_output_limit() {
    let fixture = Fixture::new("antigravity", vec![reply(3)]).await;
    {
        let mut models = fixture.state.models.write().await;
        models[0].max_output_tokens = Some(4096);
        models[0].thinking =
            Some(json!({"min":1024,"max":65535,"zero_allowed":false,"dynamic_allowed":false}));
    }
    let public: Value = fixture
        .client
        .get(format!("{}/v1/models", fixture.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(public["data"][0]["max_output_tokens"], 4096);
    let mut body = request("chat/completions", &[], "antigravity");
    body.as_object_mut().unwrap().remove("max_tokens");
    body["reasoning_effort"] = json!("high");
    // The model prefix selects Claude's budget constraint while its account
    // and synthetic upstream stay unchanged.
    {
        let mut models = fixture.state.models.write().await;
        models[0].id = "antigravity/claude-synthetic".into();
        models[0].upstream_id = "claude-synthetic".into();
    }
    body["model"] = json!("antigravity/claude-synthetic");
    let (status, body) = fixture
        .post("chat/completions", body, Some("thinking-default"), false)
        .await;
    assert_eq!(status, 200, "{body}");
    let received = fixture.mock.received.lock().unwrap();
    let generation = &received[0]["request"]["generationConfig"];
    assert_eq!(generation["maxOutputTokens"], 4096);
    assert_eq!(generation["thinkingConfig"]["thinkingBudget"], 4095);
}

fn reply(turn: usize) -> Value {
    let parts = if turn < 3 {
        json!([
            {"thought":true,"text":format!("synthetic hidden {turn}")},
            {"thoughtSignature":format!("SYNTHETIC_THOUGHT_{turn}")},
            {"text":format!("before{turn} ")},
            {"functionCall":{"name":"synthetic_tool","args":{"n":turn}},"thoughtSignature":format!("SYNTHETIC_CALL_{turn}")},
            {"text":format!("after{turn}")}
        ])
    } else {
        json!([{"text":"DONE","thoughtSignature":"SYNTHETIC_FINAL"}])
    };
    json!({"response":{"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}]}})
}

fn request(kind: &str, calls: &[String], provider: &str) -> Value {
    let mut history = vec![json!({"role":"user","content":"synthetic task"})];
    if kind == "responses" {
        history =
            vec![json!({"role":"user","content":[{"type":"input_text","text":"synthetic task"}]})];
    }
    for (n, id) in calls.iter().enumerate() {
        let visible = format!("before{} after{}", n + 1, n + 1);
        match kind {
            "chat/completions" => {
                history.push(json!({"role":"assistant","content":visible,"tool_calls":[{"id":id,"type":"function","function":{"name":"synthetic_tool","arguments":json!({"n":n+1}).to_string()}}]}));
                history.push(json!({"role":"tool","tool_call_id":id,"content":"synthetic result"}));
            }
            "messages" => {
                history.push(json!({"role":"assistant","content":[{"type":"text","text":visible},{"type":"tool_use","id":id,"name":"synthetic_tool","input":{"n":n+1}}]}));
                history.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"synthetic result"}]}));
            }
            _ => {
                history.push(
                    json!({"role":"assistant","content":[{"type":"output_text","text":visible}]}),
                );
                history.push(json!({"type":"function_call","call_id":id,"name":"synthetic_tool","arguments":json!({"n":n+1}).to_string()}));
                history.push(
                    json!({"type":"function_call_output","call_id":id,"output":"synthetic result"}),
                );
            }
        }
    }
    let identity = "You are Codex, an agent based on GPT-6.\nx-anthropic-billing-header: synthetic\nKeep tools accurate.";
    let mut result = json!({"model":format!("{provider}/synthetic-thinking"),"max_tokens":1024});
    if kind == "responses" {
        result["input"] = json!(history);
        result["instructions"] = json!(identity);
    } else {
        result["messages"] = json!(history);
        if kind == "messages" {
            result["system"] = json!(identity);
        } else {
            result["messages"]
                .as_array_mut()
                .unwrap()
                .insert(0, json!({"role":"system","content":identity}));
        }
    }
    result
}

fn call_id(kind: &str, body: &str, stream: bool) -> String {
    if !stream {
        let value: Value = serde_json::from_str(body).unwrap();
        return match kind {
            "chat/completions" => value["choices"][0]["message"]["tool_calls"][0]["id"].as_str(),
            "messages" => value["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["type"] == "tool_use")
                .unwrap()["id"]
                .as_str(),
            _ => value["output"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["type"] == "function_call")
                .unwrap()["call_id"]
                .as_str(),
        }
        .expect("native call ID exposed to client")
        .to_owned();
    }
    let mut decoder = Frames::default();
    let events = decoder.feed(body.as_bytes()).unwrap();
    events
        .iter()
        .find_map(|v| match kind {
            "chat/completions" => v
                .pointer("/choices/0/delta/tool_calls/0/id")
                .and_then(Value::as_str),
            "messages" => v.pointer("/content_block/id").and_then(Value::as_str),
            _ => v.pointer("/item/call_id").and_then(Value::as_str),
        })
        .expect("streamed native call ID exposed")
        .to_owned()
}

#[tokio::test]
async fn two_signed_tool_cycles_survive_every_client_dialect_and_transport() {
    for kind in ["chat/completions", "messages", "responses"] {
        for stream in [false, true] {
            for native_ids in [false, true] {
                let mut native_replies = vec![reply(1), reply(2), reply(3)];
                if native_ids {
                    for (index, reply) in native_replies[..2].iter_mut().enumerate() {
                        reply["response"]["candidates"][0]["content"]["parts"][3]["functionCall"]
                            ["id"] = json!(format!("provider_call_{index}"));
                    }
                }
                let fixture = Fixture::new("antigravity", native_replies.clone()).await;
                let mut ids = vec![];
                for turn in 1..=3 {
                    let (status, body) = fixture
                        .post(
                            kind,
                            request(kind, &ids, "antigravity"),
                            Some("synthetic-session"),
                            stream,
                        )
                        .await;
                    assert_eq!(status, 200, "{kind} stream={stream}: {body}");
                    if turn < 3 {
                        ids.push(call_id(kind, &body, stream));
                    } else {
                        assert!(body.contains("DONE"));
                    }
                    // Thought text may be reasoning_content, but must never become assistant content.
                    if !stream && kind == "chat/completions" {
                        let value: Value = serde_json::from_str(&body).unwrap();
                        assert!(
                            !value["choices"][0]["message"]["content"]
                                .as_str()
                                .unwrap_or("")
                                .contains("synthetic hidden")
                        );
                    }
                }
                assert_ne!(ids[0], ids[1]);
                if native_ids {
                    assert_eq!(ids, ["provider_call_0", "provider_call_1"]);
                }
                let received = fixture.mock.received.lock().unwrap();
                assert!(
                    received[0]["request"]["systemInstruction"]["parts"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("C\u{200b}odex")
                );
                assert!(
                    !received[0]["request"]["systemInstruction"]["parts"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("billing-header")
                );
                for request_index in 1..3 {
                    for native_turn in 1..=request_index {
                        let parts = &received[request_index]["request"]["contents"]
                            [native_turn * 2 - 1]["parts"];
                        assert_eq!(
                            parts,
                            &native_replies[native_turn - 1]["response"]["candidates"][0]["content"]
                                ["parts"],
                            "{kind} stream={stream}: signed native parts must be unchanged"
                        );
                        let response = &received[request_index]["request"]["contents"]
                            [native_turn * 2]["parts"][0]["functionResponse"];
                        assert_eq!(
                            response.get("id"),
                            native_replies[native_turn - 1]["response"]["candidates"][0]["content"]
                                ["parts"][3]["functionCall"]
                                .get("id"),
                            "{kind} stream={stream}: provider result identity must match the original call"
                        );
                        assert_eq!(response["name"], "synthetic_tool");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn three_native_sse_frames_replay_without_empty_text_sentinels() {
    for kind in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            let native_replies: Vec<_> = (1..=3).map(|turn| {
                let mut value = reply(turn);
                if turn < 3 {
                    value["response"]["candidates"][0]["content"]["parts"] = json!([
                        {"functionCall":{"name":"synthetic_tool","args":{"n":turn},"id":format!("toolu_fixture_{turn}")}}
                    ]);
                }
                value
            }).collect();
            let fixture = Fixture::new("antigravity", native_replies.clone()).await;
            fixture.mock.force_stream.store(true, Ordering::Relaxed);
            fixture
                .mock
                .empty_stream_sentinels
                .store(true, Ordering::Relaxed);
            let mut ids = Vec::new();
            for turn in 1..=3 {
                let mut value = request(kind, &ids, "antigravity");
                let key = if kind == "responses" {
                    "input"
                } else {
                    "messages"
                };
                let history = value[key].as_array_mut().unwrap();
                if kind == "responses" {
                    history.retain(|entry| entry["role"] != "assistant");
                } else {
                    for entry in history
                        .iter_mut()
                        .filter(|entry| entry["role"] == "assistant")
                    {
                        if kind == "messages" {
                            entry["content"]
                                .as_array_mut()
                                .unwrap()
                                .retain(|part| part["type"] != "text");
                        } else {
                            entry["content"] = Value::Null;
                        }
                    }
                }
                let (status, body) = fixture
                    .post(kind, value, Some("three-sse-frames"), stream)
                    .await;
                assert_eq!(status, 200, "{kind}, stream={stream}, turn={turn}: {body}");
                if turn < 3 {
                    ids.push(call_id(kind, &body, stream));
                } else {
                    assert!(body.contains("DONE"));
                }
            }
            let received = fixture.mock.received.lock().unwrap();
            for request_index in 1..3 {
                for native_turn in 1..=request_index {
                    assert_eq!(
                        received[request_index]["request"]["contents"][native_turn * 2 - 1]["parts"],
                        native_replies[native_turn - 1]["response"]["candidates"][0]["content"]["parts"],
                        "{kind}, stream={stream}: empty SSE sentinels are not historical text parts"
                    );
                    assert_eq!(
                        received[request_index]["request"]["contents"][native_turn * 2]["parts"][0]
                            ["functionResponse"]["id"],
                        format!("toolu_fixture_{native_turn}")
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn streamed_thought_signature_survives_two_tool_cycles() {
    for kind in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            let mut expected = Vec::new();
            let replies = (1..=3).map(|turn| {
                let mut value = reply(turn);
                if turn < 3 {
                    let call = json!({"functionCall":{
                        "name":"synthetic_tool","args":{"n":turn},"id":format!("toolu_thought_{turn}")
                    }});
                    value["response"]["candidates"][0]["content"]["parts"] = json!([
                        {"thought":true,"text":"synthetic "},
                        {"thought":true,"text":format!("reasoning {turn}")},
                        {"thought":true,"text":"","thoughtSignature":format!("SYNTHETIC_CLOSING_{turn}")},
                        {"text":format!("before{turn} ")},
                        call.clone(),
                        {"text":format!("after{turn}")}
                    ]);
                    expected.push(json!([
                        {"thought":true,"text":format!("synthetic reasoning {turn}"),"thoughtSignature":format!("SYNTHETIC_CLOSING_{turn}")},
                        {"text":format!("before{turn} ")}, call,
                        {"text":format!("after{turn}")}
                    ]));
                }
                value
            }).collect();
            let fixture = Fixture::new("antigravity", replies).await;
            fixture.mock.force_stream.store(true, Ordering::Relaxed);
            fixture
                .mock
                .empty_stream_sentinels
                .store(true, Ordering::Relaxed);
            let mut ids = Vec::new();
            for turn in 1..=3 {
                let (status, body) = fixture
                    .post(
                        kind,
                        request(kind, &ids, "antigravity"),
                        Some("closing-signature"),
                        stream,
                    )
                    .await;
                assert_eq!(status, 200, "{kind}, stream={stream}, turn={turn}: {body}");
                if turn < 3 {
                    ids.push(call_id(kind, &body, stream));
                } else {
                    assert!(body.contains("DONE"));
                }
            }
            assert_eq!(ids, ["toolu_thought_1", "toolu_thought_2"]);
            let received = fixture.mock.received.lock().unwrap();
            for request_index in 1..3 {
                for native_turn in 1..=request_index {
                    assert_eq!(
                        received[request_index]["request"]["contents"][native_turn * 2 - 1]["parts"],
                        expected[native_turn - 1],
                        "{kind}, stream={stream}: a closing signature belongs to the preceding thought block"
                    );
                    assert_eq!(
                        received[request_index]["request"]["contents"][native_turn * 2]["parts"][0]
                            ["functionResponse"]["id"],
                        format!("toolu_thought_{native_turn}")
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn interrupted_native_stream_never_commits_a_signed_tool_turn() {
    let mut incomplete = reply(1);
    incomplete["response"]["candidates"][0]
        .as_object_mut()
        .unwrap()
        .remove("finishReason");
    let fixture = Fixture::new("antigravity", vec![incomplete]).await;
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[], "antigravity"),
            Some("interrupted"),
            true,
        )
        .await;
    assert_eq!(status, 200);
    assert!(body.contains("event: error"));
    let id = call_id("chat/completions", &body, true);
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[id], "antigravity"),
            Some("interrupted"),
            false,
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(fixture.mock.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn anonymous_tool_history_fails_clearly_without_borrowing_another_session() {
    let fixture = Fixture::new("antigravity", vec![reply(1)]).await;
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[], "antigravity"),
            None,
            false,
        )
        .await;
    assert_eq!(status, 200);
    let id = call_id("chat/completions", &body, false);
    let (status, body) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[id], "antigravity"),
            None,
            false,
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("session"));
    assert_eq!(fixture.mock.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn other_google_routes_preserve_the_harness_system_text() {
    let fixture = Fixture::new("gemini", vec![reply(3)]).await;
    let (status, _) = fixture
        .post(
            "chat/completions",
            request("chat/completions", &[], "gemini"),
            Some("other-route"),
            false,
        )
        .await;
    assert_eq!(status, 200);
    let received = fixture.mock.received.lock().unwrap();
    let text = received[0]["request"]["systemInstruction"]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("You are Codex"));
    assert!(text.contains("billing-header"));
}

// Claude Code puts its environment and token reminders inside messages, even
// though the root system prompt remains unchanged between requests.
fn with_system_reminders(kind: &str, calls: &[String]) -> Value {
    let mut body = request(kind, calls, "antigravity");
    let key = if kind == "responses" {
        "input"
    } else {
        "messages"
    };
    let history = body[key].as_array_mut().unwrap();
    let first = if kind == "chat/completions" { 2 } else { 1 };
    let environment = "Synthetic environment: preserve this instruction.";
    let content = if calls.is_empty() {
        json!([{"type":if kind == "responses" {"input_text"} else {"text"},"text":environment,"cache_control":{"type":"ephemeral"}}])
    } else {
        json!(environment)
    };
    history.insert(first, json!({"role":"system","content":content}));
    let stride = if kind == "responses" { 3 } else { 2 };
    for turn in 0..calls.len() {
        let index = first + 1 + (turn + 1) * stride + turn;
        history.insert(
            index,
            json!({"role":"system","content":format!("Synthetic token reminder {}", turn + 1)}),
        );
    }
    body
}

#[tokio::test]
async fn dynamic_system_reminders_keep_signed_history_in_every_dialect() {
    for kind in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            let fixture = Fixture::new("antigravity", vec![reply(1), reply(2), reply(3)]).await;
            let mut ids = Vec::new();
            for turn in 0..3 {
                let (status, body) = fixture
                    .post(
                        kind,
                        with_system_reminders(kind, &ids),
                        Some("dynamic-system"),
                        stream,
                    )
                    .await;
                assert_eq!(status, 200, "{kind}, stream={stream}, turn={turn}: {body}");
                if turn < 2 {
                    ids.push(call_id(kind, &body, stream));
                }
            }
            {
                let received = fixture.mock.received.lock().unwrap();
                let last = &received[2]["request"];
                let system = last["systemInstruction"]["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["text"].as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(system.contains("Synthetic environment: preserve this instruction."));
                assert!(system.contains("Synthetic token reminder 1"));
                assert!(system.contains("Synthetic token reminder 2"));
                assert!(
                    last["contents"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|c| c["role"] != "system")
                );
                for turn in 0..2 {
                    assert_eq!(
                        last["contents"][1 + turn * 2]["parts"][3]["thoughtSignature"],
                        format!("SYNTHETIC_CALL_{}", turn + 1)
                    );
                }
            }
            // Rewriting a reminder that preceded an already signed call must
            // still refuse the entire request before reaching the provider.
            let key = if kind == "responses" {
                "input"
            } else {
                "messages"
            };
            let first = if kind == "chat/completions" { 2 } else { 1 };
            let stride = if kind == "responses" { 3 } else { 2 };
            for index in [first, first + 1 + stride] {
                let mut changed = with_system_reminders(kind, &ids);
                changed[key][index]["content"] = json!("Changed historical instruction");
                let (status, body) = fixture
                    .post(kind, changed, Some("dynamic-system"), stream)
                    .await;
                assert_eq!(status, 400, "{kind}, index={index}: {body}");
                assert_eq!(fixture.mock.received.lock().unwrap().len(), 3);
            }
        }
    }
}

#[tokio::test]
async fn text_correction_with_new_system_reminders_restores_hidden_native_parts() {
    let signed = json!([
        {"thought":true,"text":"synthetic private reasoning","thoughtSignature":"SYNTHETIC_THOUGHT"},
        {"text":"first answer","thoughtSignature":"SYNTHETIC_TEXT"}
    ]);
    for kind in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            let mut first = reply(3);
            first["response"]["candidates"][0]["content"]["parts"] = json!([
                {"thought":true,"text":"synthetic private "},
                {"thought":true,"text":"reasoning"},
                {"thought":true,"text":"","thoughtSignature":"SYNTHETIC_THOUGHT"},
                {"text":"first answer","thoughtSignature":"SYNTHETIC_TEXT"}
            ]);
            let fixture = Fixture::new("antigravity", vec![first, reply(3)]).await;
            fixture.mock.force_stream.store(true, Ordering::Relaxed);
            fixture
                .mock
                .empty_stream_sentinels
                .store(true, Ordering::Relaxed);
            let (status, body) = fixture
                .post(
                    kind,
                    with_system_reminders(kind, &[]),
                    Some("text-correction"),
                    stream,
                )
                .await;
            assert_eq!(status, 200, "{body}");
            let mut next = with_system_reminders(kind, &[]);
            let key = if kind == "responses" {
                "input"
            } else {
                "messages"
            };
            next[key].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":"first answer"}),
                json!({"role":"user","content":"Please return valid JSON."}),
                json!({"role":"system","content":"Synthetic token reminder 1"}),
            ]);
            let (status, body) = fixture
                .post(kind, next, Some("text-correction"), stream)
                .await;
            assert_eq!(status, 200, "{body}");
            let received = fixture.mock.received.lock().unwrap();
            assert_eq!(received[1]["request"]["contents"][1]["parts"], signed);
        }
    }
}

#[tokio::test]
async fn unreplayable_native_turn_is_an_error_before_successful_completion() {
    for stream in [false, true] {
        let mut oversized = reply(3);
        oversized["response"]["candidates"][0]["content"]["parts"] =
            json!([{"text":"x".repeat(256 * 1024 + 1),"thoughtSignature":"SYNTHETIC_OVERSIZED"}]);
        let fixture = Fixture::new("antigravity", vec![oversized]).await;
        let (status, body) = fixture
            .post(
                "messages",
                request("messages", &[], "antigravity"),
                Some("over-capacity"),
                stream,
            )
            .await;
        if stream {
            assert_eq!(status, 200);
            assert!(
                body.contains("event: error"),
                "unrecordable native output must be an explicit error"
            );
            assert!(
                !body.contains("event: message_stop"),
                "a recording failure must never look like success"
            );
        } else {
            assert_eq!(
                status, 502,
                "unrecordable native output must be an explicit error"
            );
            assert!(body.contains("signed Gemini history"));
        }
    }
}

#[tokio::test]
async fn full_conversation_budget_refuses_the_current_completion_explicitly() {
    for stream in [false, true] {
        let replies = (0..80).map(|turn| {
            let mut native = reply(3);
            native["response"]["candidates"][0]["content"]["parts"] = json!([
                {"thought":true,"text":"x".repeat(192 * 1024),"thoughtSignature":"SYNTHETIC_HIDDEN"},
                {"text":format!("answer {turn}"),"thoughtSignature":"SYNTHETIC_TEXT"}
            ]);
            native
        }).collect();
        let fixture = Fixture::new("antigravity", replies).await;
        let mut history = with_system_reminders("messages", &[]);
        let mut refused = false;
        for turn in 0..80 {
            let (status, body) = fixture
                .post(
                    "messages",
                    history.clone(),
                    Some("full-conversation"),
                    stream,
                )
                .await;
            if body.contains("byte budget") {
                assert_eq!(status, if stream { 200 } else { 502 });
                if stream {
                    assert!(body.contains("event: error"));
                    assert!(!body.contains("event: message_stop"));
                }
                refused = true;
                break;
            }
            assert_eq!(status, 200, "{body}");
            assert!(!body.contains("event: error"), "{body}");
            history["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":format!("answer {turn}")}),
                json!({"role":"user","content":"Continue the synthetic conversation."}),
            ]);
        }
        assert!(
            refused,
            "the unchanged 8 MiB budget must refuse this conversation before losing its prefix"
        );
    }
}

#[tokio::test]
async fn replay_owner_waits_through_rate_limits_without_switching_accounts() {
    for kind in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            let fixture = Fixture::new("antigravity", vec![reply(1), reply(3)]).await;
            let owner = fixture.state.accounts.list().await[0].id.clone();
            let (status, body) = fixture
                .post(
                    kind,
                    request(kind, &[], "antigravity"),
                    Some("rate-owner"),
                    stream,
                )
                .await;
            assert_eq!(status, 200, "{body}");
            let id = call_id(kind, &body, stream);
            let alternate = Arc::new(Mock::default());
            let (upstream, task) = serve(
                Router::new()
                    .route("/v1internal:generateContent", post(native))
                    .route("/v1internal:streamGenerateContent", post(native_stream))
                    .with_state(alternate.clone()),
            )
            .await;
            let imported = fixture.state.accounts.import(json!({"type":"antigravity","access_token":"synthetic-alternate","base_url":upstream,"models":["synthetic-thinking","claude-synthetic"]}),None).await.unwrap();
            let other = imported.id.clone();
            fixture.state.refresh_models().await;
            fixture.mock.refusal_status.store(429, Ordering::Relaxed);
            let follow = request(kind, &[id], "antigravity");
            let send = || {
                fixture
                    .client
                    .post(format!("{}/v1/{kind}", fixture.url))
                    .header("x-opencode-session-id", "rate-owner")
                    .json(&{
                        let mut value = follow.clone();
                        value["stream"] = json!(stream);
                        value
                    })
            };
            for attempt in 0..2 {
                let response = send().send().await.unwrap();
                let status = response.status();
                let retry = response.headers().get("retry-after").cloned();
                let body = response.text().await.unwrap();
                assert_eq!(
                    status, 429,
                    "{kind} stream={stream} attempt={attempt}: {body}"
                );
                assert!(retry.unwrap().to_str().unwrap().parse::<u64>().unwrap() > 0);
                assert!(!body.contains("does not match"));
                assert_eq!(alternate.received.lock().unwrap().len(), 0);
                assert_eq!(fixture.mock.received.lock().unwrap().len(), 2);
            }
            // A fresh conversation can still use the other eligible account.
            let (status, body) = fixture
                .post(
                    kind,
                    request(kind, &[], "antigravity"),
                    Some("fresh-rate-session"),
                    stream,
                )
                .await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(alternate.received.lock().unwrap().len(), 1);
            fixture.mock.refusal_status.store(0, Ordering::Relaxed);
            fixture
                .state
                .accounts
                .cooldown(&owner, 0, "synthetic cooldown elapsed")
                .await
                .unwrap();
            // An unrelated scheduler pin must not move already recorded history.
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("x-opencode-session-id", "rate-owner".parse().unwrap());
            let session = douane::affinity::session_key(&headers, &json!({})).unwrap();
            fixture
                .state
                .affinity
                .lock()
                .unwrap()
                .bind("antigravity", &session, &other);
            let response = send().send().await.unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(status, 200, "{body}");
            assert_eq!(fixture.mock.received.lock().unwrap().len(), 3);
            assert_eq!(alternate.received.lock().unwrap().len(), 1);
            task.abort();
        }
    }
}

#[tokio::test]
async fn replay_owner_cooldown_never_bypasses_access_or_revocation() {
    let fixture = Fixture::new("antigravity", vec![reply(1)]).await;
    let owner = fixture.state.accounts.list().await[0].id.clone();
    let (status, body) = fixture
        .post(
            "messages",
            request("messages", &[], "antigravity"),
            Some("owner-access"),
            false,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let id = call_id("messages", &body, false);
    let follow = request("messages", &[id], "antigravity");
    fixture
        .state
        .accounts
        .cooldown(&owner, 60, "synthetic cooldown")
        .await
        .unwrap();
    fixture
        .state
        .accounts
        .set_enabled(&owner, false)
        .await
        .unwrap();
    let (status, body) = fixture
        .post("messages", follow.clone(), Some("owner-access"), false)
        .await;
    assert_eq!(status, 503, "disabled owner: {body}");
    fixture
        .state
        .accounts
        .set_enabled(&owner, true)
        .await
        .unwrap();
    let client = fixture.state.clients.list()[0].id.clone();
    fixture
        .state
        .clients
        .update(
            &client,
            douane::clients::ClientPatch {
                access: Some(douane::clients::Access {
                    all: false,
                    pools: Default::default(),
                    accounts: Default::default(),
                }),
                ..Default::default()
            },
        )
        .unwrap();
    let (status, body) = fixture
        .post("messages", follow.clone(), Some("owner-access"), false)
        .await;
    assert_eq!(status, 503, "denied owner: {body}");
    fixture
        .state
        .clients
        .update(
            &client,
            douane::clients::ClientPatch {
                access: Some(Default::default()),
                ..Default::default()
            },
        )
        .unwrap();
    fixture.state.accounts.delete(&owner).await.unwrap();
    let (status, body) = fixture
        .post("messages", follow, Some("owner-access"), false)
        .await;
    assert_eq!(status, 503, "deleted owner: {body}");
    assert_eq!(fixture.mock.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn replay_owner_refusals_never_fall_back_to_another_account() {
    for refused in [401, 404, 503] {
        let fixture = Fixture::new("antigravity", vec![reply(1)]).await;
        let (status, body) = fixture
            .post(
                "messages",
                request("messages", &[], "antigravity"),
                Some("owner-refusal"),
                false,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let id = call_id("messages", &body, false);
        let alternate = Arc::new(Mock::default());
        let (upstream, task) = serve(
            Router::new()
                .route("/v1internal:generateContent", post(native))
                .with_state(alternate.clone()),
        )
        .await;
        fixture.state.accounts.import(json!({"type":"antigravity","access_token":"synthetic-fallback","base_url":upstream,"models":["synthetic-thinking"]}),None).await.unwrap();
        fixture.state.refresh_models().await;
        fixture
            .mock
            .refusal_status
            .store(refused, Ordering::Relaxed);
        let (status, body) = fixture
            .post(
                "messages",
                request("messages", &[id], "antigravity"),
                Some("owner-refusal"),
                false,
            )
            .await;
        assert_eq!(status, refused, "{body}");
        assert!(!body.contains("does not match"));
        assert_eq!(fixture.mock.received.lock().unwrap().len(), 2);
        assert!(alternate.received.lock().unwrap().is_empty());
        task.abort();
    }
}
