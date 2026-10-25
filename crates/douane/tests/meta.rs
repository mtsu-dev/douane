use douane::subscriptions::{AccountStore, OAuthManager, SubscriptionProvider};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn native_meta_subscription_is_importable_and_private() {
    let provider = SubscriptionProvider::parse("meta").expect("native Muse Code provider");
    assert_eq!(provider.as_str(), "meta");
    assert!(
        provider.reports_quota(),
        "quotas are observed from native subscription telemetry"
    );
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(dir.path()).unwrap());
    assert!(OAuthManager::new(reqwest::Client::new(), store.clone()).supports(provider));
    let account = store.import(json!({"type":"meta","api_key":"minted-fixture","dca_token":"dca:fixture","email":"fixture@example.test","subs_tier_name":"Contributor","subs_tier_id":"contributor","base_url":"https://api.meta.ai/v1"}), None).await.unwrap();
    let public = serde_json::to_string(&account).unwrap();
    assert!(!public.contains("minted-fixture") && !public.contains("dca:fixture"));
    assert_eq!(account.subscription.unwrap().label, "Contributor");
    assert_eq!(
        AccountStore::open(dir.path()).unwrap().list().await.len(),
        1
    );
}

#[tokio::test]
async fn meta_models_are_discovered_and_routed_per_account() {
    use axum::{Json, Router, routing::get};
    use douane::{settings::Config, state::AppState};
    let app = Router::new()
        .route(
            "/first/muse-code/models",
            get(|| async { Json(json!({"data":[{"id":"muse-first"},{"id":"filtered"}]})) }),
        )
        .route(
            "/second/muse-code/models",
            get(|| async { Json(json!({"data":[{"id":"muse-second"}]})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.auth_dir = dir.path().join("auth");
    config.server.database = dir.path().join("calls.sqlite");
    let state = AppState::new(config, None).unwrap();
    for (name, models) in [
        ("first", json!(["muse-first"])),
        ("second", json!([])),
        ("unlisted", json!([])),
    ] {
        state.accounts.import(json!({"type":"meta","api_key":format!("key-{name}"),"email":format!("{name}@example.test"),"base_url":format!("{base}/{name}"),"models":models}),Some(name.into())).await.unwrap();
    }
    state.refresh_models().await;
    for name in ["first", "second"] {
        let (_, accounts) = state
            .candidates(&format!("meta/muse-{name}"), None, None)
            .await
            .unwrap();
        assert_eq!(accounts.len(), 1);
        // Public summaries identify the selected account without credentials.
        let public = state.accounts.list().await;
        let exported = state.accounts.export().await;
        let selected = serde_json::to_value(&accounts[0]).unwrap();
        assert_eq!(
            public.iter().find(|a| a.id == selected["id"]).unwrap().name,
            name
        );
        assert_eq!(exported.len(), 3);
    }
    assert!(state.candidates("meta/filtered", None, None).await.is_err());
    for account in state.accounts.list().await {
        let snapshot = state.quotas.refresh(&account.id).await.unwrap();
        assert!(snapshot.windows.is_empty() && snapshot.error.is_none());
    }
    assert!(
        state
            .quotas
            .due(time::Duration::ZERO, time::OffsetDateTime::now_utc())
            .await
            .is_empty()
    );
    server.abort();
}

#[tokio::test]
async fn meta_import_refuses_inactive_or_payment_required_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let store = AccountStore::open(dir.path()).unwrap();
    for status in [
        json!({"is_subs_active":false}),
        json!({"is_subs_active":true,"require_payment":true}),
    ] {
        let mut record =
            json!({"type":"meta","api_key":"private-api-key","dca_token":"dca:fixture"});
        record
            .as_object_mut()
            .unwrap()
            .extend(status.as_object().unwrap().clone());
        let nested = json!({"type":"meta","token":record.clone()});
        assert!(store.import(record, None).await.is_err());
        assert!(store.import(nested, None).await.is_err());
    }
    assert!(store.list().await.is_empty());
}
