//! Antigravity accounts of one pool that do not offer the same models.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use douane::{
    scheduler::Strategy, settings::Config, state::AppState, subscriptions::SubscriptionProvider,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

/// One Google account: the models its listing names, the ones its
/// generation endpoint actually serves, and the models it was asked for.
struct Plan {
    listed: Vec<&'static str>,
    served: Vec<&'static str>,
    asked: Mutex<Vec<String>>,
}

impl Plan {
    fn new(listed: &[&'static str], served: &[&'static str]) -> Arc<Self> {
        Arc::new(Self {
            listed: listed.to_vec(),
            served: served.to_vec(),
            asked: Mutex::new(vec![]),
        })
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

/// The plan an upstream answers for, bound once the account order is known.
type Slot = Arc<Mutex<Option<Arc<Plan>>>>;

fn plan_in(slot: &Slot) -> Arc<Plan> {
    slot.lock()
        .unwrap()
        .clone()
        .expect("plan bound before any call")
}

// Google withholds Argon from the legacy Hub client identity.
fn current_antigravity_client(headers: &HeaderMap) -> bool {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        std::env::consts::ARCH
    };
    let expected = format!(
        "antigravity/acp/1.4.1 (aidev_client; os_type={os}; arch={arch}; host_path=douane/{}; proxy_client=antigravity/sdk)",
        env!("CARGO_PKG_VERSION")
    );
    headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        == Some(expected.as_str())
}

async fn listing(State(slot): State<Slot>, headers: HeaderMap) -> Json<Value> {
    let plan = plan_in(&slot);
    let models: serde_json::Map<String, Value> = plan
        .listed
        .iter()
        .filter(|id| !id.starts_with("gemini-4-argon-") || current_antigravity_client(&headers))
        .map(|id| {
            (
                (*id).to_owned(),
                json!({"displayName": id, "maxTokens": 200000}),
            )
        })
        .collect();
    Json(json!({"models": models}))
}

async fn generate(
    State(slot): State<Slot>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let plan = plan_in(&slot);
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    plan.asked.lock().unwrap().push(model.clone());
    if model.starts_with("gemini-4-argon-") && !current_antigravity_client(&headers) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !plan.served.contains(&model.as_str()) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":{"code":404,"message":"Requested entity was not found.","status":"NOT_FOUND"}})),
        )
            .into_response();
    }
    Json(json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"OK"}]},"finishReason":"STOP"}]}}))
        .into_response()
}

async fn google(slot: Slot) -> (String, JoinHandle<()>) {
    let router = Router::new()
        .route("/v1internal:fetchAvailableModels", post(listing))
        .route("/v1internal:generateContent", post(generate))
        .with_state(slot);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }),
    )
}

struct Pool {
    state: Arc<AppState>,
    url: String,
    client: reqwest::Client,
    /// The account fill-first tries first, then the second one.
    first: (String, Arc<Plan>),
    second: (String, Arc<Plan>),
    tasks: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Pool {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Pool {
    /// Two accounts on fill-first, so the account tried first is known:
    /// `first` gets the plan fill-first would pick, `second` the other one.
    async fn new(first: Arc<Plan>, second: Arc<Plan>) -> Self {
        let slots: [Slot; 2] = Default::default();
        let (first_url, first_task) = google(slots[0].clone()).await;
        let (second_url, second_task) = google(slots[1].clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = dir.path().join("auth");
        config.server.database = dir.path().join("calls.sqlite");
        config.settings.strategy = Strategy::FillFirst;
        let state = AppState::new(config, None).unwrap();
        let (_, key) = state
            .clients
            .create(douane::clients::ClientPatch {
                name: Some("tests".into()),
                ..Default::default()
            })
            .unwrap();
        let mut ids = vec![];
        for (n, url) in [&first_url, &second_url].into_iter().enumerate() {
            let account = state
                .accounts
                .import(
                    json!({"type":"antigravity","access_token":format!("synthetic-{n}"),
                        "email":format!("synthetic-{n}@example.test"),"project_id":"synthetic-project",
                        "base_url":url}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        // Fill-first walks accounts by id: the smaller id gets the first plan.
        let order = if ids[0] < ids[1] { [0, 1] } else { [1, 0] };
        *slots[order[0]].lock().unwrap() = Some(first.clone());
        *slots[order[1]].lock().unwrap() = Some(second.clone());
        let (first_id, second_id) = (ids[order[0]].clone(), ids[order[1]].clone());
        state.refresh_models().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = douane::router(state.clone());
        let gate_task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            state,
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
            first: (first_id, first),
            second: (second_id, second),
            tasks: vec![first_task, second_task, gate_task],
            _dir: dir,
        }
    }

    async fn ask(&self, model: &str) -> (u16, String) {
        let response = self
            .client
            .post(format!("{}/v1/messages", self.url))
            .header("user-agent", "unrelated-client/1.0")
            .json(&json!({"model":model,"max_tokens":64,
                "messages":[{"role":"user","content":"Reply OK"}]}))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn status(&self, id: &str) -> String {
        let accounts = self.state.accounts.list().await;
        accounts.into_iter().find(|a| a.id == id).unwrap().status
    }
}

#[tokio::test]
async fn argon_is_discovered_and_relayed_with_the_current_client_identity() {
    let argon = [
        "gemini-4-argon-high",
        "gemini-4-argon-medium",
        "gemini-4-argon-low",
    ];
    let pool = Pool::new(
        Plan::new(&["gemini-2.5-pro"], &["gemini-2.5-pro"]),
        Plan::new(&argon, &argon),
    )
    .await;
    let response = pool
        .client
        .get(format!("{}/v1/models", pool.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let catalog: Value = response.json().await.unwrap();
    for model in argon {
        let id = format!("antigravity/{model}");
        assert!(
            catalog["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["id"] == id),
            "missing {id}: {catalog}"
        );
        let (status, body) = pool.ask(&id).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("OK"), "{body}");
    }
    assert!(pool.first.1.asked().is_empty());
    assert_eq!(pool.second.1.asked(), argon);
}

#[tokio::test]
async fn a_model_goes_only_to_the_accounts_whose_listing_names_it() {
    let pool = Pool::new(
        Plan::new(&["claude-old"], &["claude-old"]),
        Plan::new(&["claude-new", "claude-old"], &["claude-new", "claude-old"]),
    )
    .await;
    let catalog: Vec<String> = pool
        .state
        .models
        .read()
        .await
        .iter()
        .filter(|m| m.provider == SubscriptionProvider::Antigravity)
        .map(|m| m.id.clone())
        .collect();
    assert!(
        catalog.contains(&"antigravity/claude-new".to_owned()),
        "{catalog:?}"
    );
    for _ in 0..3 {
        let (status, body) = pool.ask("antigravity/claude-new").await;
        assert_eq!(status, 200, "{body}");
    }
    assert!(
        pool.first.1.asked().is_empty(),
        "{:?}",
        pool.first.1.asked()
    );
    assert_eq!(pool.second.1.asked().len(), 3);
    // The model both plans list still goes to the first account.
    let (status, body) = pool.ask("antigravity/claude-old").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(pool.first.1.asked(), ["claude-old"]);
}

#[tokio::test]
async fn a_not_found_moves_to_the_next_account_without_an_error() {
    // The first plan lists the model yet its endpoint does not serve it.
    let pool = Pool::new(
        Plan::new(&["claude-new"], &[]),
        Plan::new(&["claude-new"], &["claude-new"]),
    )
    .await;
    let (status, body) = pool.ask("antigravity/claude-new").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(pool.first.1.asked(), ["claude-new"]);
    assert_eq!(pool.second.1.asked(), ["claude-new"]);
    assert_eq!(pool.status(&pool.first.0).await, "ready");
    // The refusal is remembered: the next request skips the first account.
    let (status, body) = pool.ask("antigravity/claude-new").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(pool.first.1.asked().len(), 1);
    assert_eq!(pool.second.1.asked().len(), 2);
}

#[tokio::test]
async fn a_model_no_account_serves_leaves_the_catalog() {
    let pool = Pool::new(
        Plan::new(&["claude-new"], &[]),
        Plan::new(&["claude-new"], &[]),
    )
    .await;
    let (status, body) = pool.ask("antigravity/claude-new").await;
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("does not serve this model"), "{body}");
    assert_eq!(pool.status(&pool.first.0).await, "ready");
    assert_eq!(pool.status(&pool.second.0).await, "ready");
    let listed =
        |models: &[douane::state::Model]| models.iter().any(|m| m.id == "antigravity/claude-new");
    assert!(!listed(&pool.state.models.read().await));
    // The hourly reread still finds it in both listings: it stays out.
    pool.state.refresh_models().await;
    assert!(!listed(&pool.state.models.read().await));
}
