use axum::{body::Body, http::Request};
use douane::{settings::Config, state::AppState};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn provider_pace_is_opt_in_and_persists_through_partial_settings_updates() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("douane.toml");
    let mut config = Config::default();
    config.server.auth_dir = directory.path().join("auth");
    config.server.database = directory.path().join("calls.sqlite");
    let state = AppState::new(config, Some(path.clone())).unwrap();
    let app = douane::router(state);
    for (method, patch, expected) in [
        ("GET", None, false),
        ("PATCH", Some(json!({"show_provider_pace":true})), true),
        ("PATCH", Some(json!({"quota_refresh_seconds":600})), true),
        ("PATCH", Some(json!({"show_provider_pace":false})), false),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/admin/api/settings")
                    .header("content-type", "application/json")
                    .body(patch.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let settings: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(settings["show_provider_pace"], expected);
        if method == "PATCH" {
            let saved = serde_json::to_value(Config::load(&path).unwrap().settings).unwrap();
            assert_eq!(saved["show_provider_pace"], expected);
        }
    }
}
