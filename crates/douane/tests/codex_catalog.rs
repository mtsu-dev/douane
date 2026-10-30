//! Native Codex discovery must follow each subscription's current model list.
use axum::{
    Json, Router,
    extract::{Query, State},
    http::HeaderMap,
    routing::get,
};
use douane::{settings::Config, state::AppState};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::{net::TcpListener, task::JoinHandle};

struct Fixture {
    state: Arc<AppState>,
    replies: Arc<Mutex<HashMap<String, Value>>>,
    accounts: Vec<String>,
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

impl Fixture {
    async fn new(replies: Vec<Value>) -> Self {
        let replies: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(
            replies
                .into_iter()
                .enumerate()
                .map(|(i, reply)| (i.to_string(), reply))
                .collect(),
        ));
        let router = Router::new()
            .route(
                "/models",
                get(
                    |State(replies): State<Arc<Mutex<HashMap<String, Value>>>>,
                     Query(query): Query<HashMap<String, String>>,
                     headers: HeaderMap| async move {
                        assert_eq!(query["client_version"], "0.166.0");
                        assert_eq!(headers["originator"], "codex_cli_rs");
                        assert_eq!(headers["user-agent"], "codex_cli_rs/0.166.0");
                        let id = headers["chatgpt-account-id"].to_str().unwrap();
                        assert_eq!(headers["authorization"], format!("Bearer fixture-{id}"));
                        Json(replies.lock().unwrap()[id].clone())
                    },
                ),
            )
            .with_state(replies.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = dir.path().join("auth");
        config.server.database = dir.path().join("calls.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut accounts = vec![];
        for i in 0..replies.lock().unwrap().len() {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex",
                "access_token":format!("fixture-{i}"),"account_id":i.to_string(),
                "base_url":base}),
                    None,
                )
                .await
                .unwrap();
            accounts.push(account.id);
        }
        Self {
            state,
            replies,
            accounts,
            tasks: vec![task],
            _dir: dir,
        }
    }

    async fn ids(&self) -> Vec<String> {
        self.state
            .models
            .read()
            .await
            .iter()
            .map(|m| m.id.clone())
            .collect()
    }
}

fn listing(id: &str, context: u64) -> Value {
    json!({"models":[{"slug":id,"display_name":"Native model", "context_window":context,
        "supported_in_api":true,"supported_reasoning_levels":[{"effort":"low"},{"effort":"ultra"}]},
        {"slug":"unsupported-chat","supported_in_api":false}, {}]})
}

#[tokio::test]
async fn native_codex_models_are_discovered_with_current_metadata_and_account_eligibility() {
    let f = Fixture::new(vec![
        listing("gpt-6.1-terra", 500000),
        listing("gpt-future", 700000),
    ])
    .await;
    f.state.refresh_models().await;
    let ids = f.ids().await;
    assert!(ids.contains(&"codex/gpt-6.1-terra".into()), "{ids:?}");
    assert!(ids.contains(&"codex/gpt-future".into()), "{ids:?}");
    assert!(!ids.contains(&"codex/unsupported-chat".into()));
    assert!(!ids.contains(&"codex/gpt-6-astra".into()));
    let models = f.state.models.read().await;
    let future = models.iter().find(|m| m.id == "codex/gpt-future").unwrap();
    assert_eq!(future.context_length, Some(700000));
    assert_eq!(
        future.thinking.as_ref().unwrap()["levels"],
        json!(["low", "ultra"])
    );
    assert_eq!(future.name, "Native model");
    drop(models);
    let (_, accounts) = f
        .state
        .candidates("gpt-6.1-terra", Some("codex"), None)
        .await
        .unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        serde_json::to_value(&accounts[0]).unwrap()["id"],
        f.accounts[0]
    );

    // Models added later become visible without a new bundled registry.
    f.replies
        .lock()
        .unwrap()
        .insert("1".into(), listing("gpt-next-release", 800000));
    f.state.refresh_models_in(Some("codex")).await;
    let ids = f.ids().await;
    assert!(ids.contains(&"codex/gpt-next-release".into()));
    assert!(!ids.contains(&"codex/gpt-future".into()));
}

#[tokio::test]
async fn failed_native_discovery_keeps_previous_models_and_successful_empty_listing_clears_them() {
    let f = Fixture::new(vec![json!({"error":"unavailable"})]).await;
    f.state.refresh_models().await;
    assert!(f.ids().await.contains(&"codex/gpt-6.1-terra".into()));
    assert!(f.state.model_listings.lock().unwrap()["codex"].stale);
    f.replies
        .lock()
        .unwrap()
        .insert("0".into(), listing("gpt-future", 700000));
    f.state.refresh_models().await;
    assert_eq!(f.ids().await, vec!["codex/gpt-future"]);
    f.replies
        .lock()
        .unwrap()
        .insert("0".into(), json!({"models":"malformed"}));
    f.state.refresh_models().await;
    assert_eq!(f.ids().await, vec!["codex/gpt-future"]);
    assert!(f.state.model_listings.lock().unwrap()["codex"].stale);
    f.replies
        .lock()
        .unwrap()
        .insert("0".into(), json!({"models":[]}));
    f.state.refresh_models().await;
    assert!(f.ids().await.is_empty());
    assert!(!f.state.model_listings.lock().unwrap()["codex"].stale);
    assert!(
        f.state
            .candidates("gpt-future", Some("codex"), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_refresh_keeps_model_disables_and_account_allowlists() {
    let f = Fixture::new(vec![listing("gpt-6.1-terra", 500000)]).await;
    f.state
        .config
        .write()
        .await
        .disabled_models
        .insert("codex/gpt-6.1-terra".into());
    f.state.refresh_models().await;
    assert!(
        !f.state
            .models
            .read()
            .await
            .iter()
            .find(|m| m.id == "codex/gpt-6.1-terra")
            .unwrap()
            .enabled
    );
    assert!(
        f.state
            .candidates("gpt-6.1-terra", Some("codex"), None)
            .await
            .is_err()
    );
    // A successful listing must not override an explicit imported allowlist.
    let mut account =
        serde_json::to_value(f.state.accounts.get(&f.accounts[0]).await.unwrap()).unwrap();
    account["models"] = json!(["gpt-restricted"]);
    f.state.accounts.import(account, None).await.unwrap();
    // The imported account is the same identity; its explicit list is retained.
    f.state.refresh_models().await;
    assert!(!f.ids().await.contains(&"codex/gpt-6.1-terra".into()));
}
