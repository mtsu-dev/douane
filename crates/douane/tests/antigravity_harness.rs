//! Synthetic native upstreams exercise complete signed tool history through the public API.
use axum::{Json, Router, body::Body, extract::State, response::IntoResponse, routing::post};
use douane::{settings::Config, state::AppState, translate::Frames};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Default)]
struct Mock {
    received: Mutex<Vec<Value>>,
    replies: Mutex<VecDeque<Value>>,
    force_stream: AtomicBool,
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
    for part in parts {
        let event =
            json!({"response":{"candidates":[{"content":{"role":"model","parts":[part]}}]}});
        data.push_str(&format!("data: {event}\n\n"));
    }
    if let Some(reason) = reply.pointer("/response/candidates/0/finishReason") {
        let event = json!({"response":{"candidates":[{"finishReason":reason}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3}}});
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
            let fixture = Fixture::new("antigravity", vec![reply(1), reply(2), reply(3)]).await;
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
            for (index, id) in ids.iter().enumerate() {
                let parts = &received[2]["request"]["contents"][1 + index * 2]["parts"];
                assert_eq!(parts[0]["thought"], true);
                assert_eq!(
                    parts[1]["thoughtSignature"],
                    format!("SYNTHETIC_THOUGHT_{}", index + 1)
                );
                assert_eq!(parts[3]["functionCall"]["id"], *id);
                assert_eq!(
                    parts[3]["thoughtSignature"],
                    format!("SYNTHETIC_CALL_{}", index + 1)
                );
                assert_eq!(
                    received[2]["request"]["contents"][2 + index * 2]["parts"][0]["functionResponse"]
                        ["id"],
                    *id
                );
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
