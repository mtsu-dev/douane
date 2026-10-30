//! Google's installed-app OAuth and Code Assist account onboarding.
//! Public-client metadata comes from private runtime configuration, not source.

use super::oauth::{google_verification_error, send_json};
use super::store::text;
use super::{SubscriptionError, SubscriptionProvider, timestamp};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::OnceLock, time::Duration};

const ANTIGRAVITY_VERSION: &str = "1.4.1";

/// Match the current ACP client protocol: Google gates models on this identity.
/// Keep Douane's host and platform explicit, and use it for every Code Assist call.
pub(crate) fn antigravity_user_agent() -> &'static str {
    static USER_AGENT: OnceLock<String> = OnceLock::new();
    USER_AGENT.get_or_init(|| {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            arch => arch,
        };
        format!(
            "antigravity/acp/{ANTIGRAVITY_VERSION} (aidev_client; os_type={os}; arch={arch}; host_path=douane/{}; proxy_client=antigravity/sdk)",
            env!("CARGO_PKG_VERSION")
        )
    })
}

const ANTIGRAVITY_CLIENT: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const GEMINI_CLIENT: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";

#[derive(Clone)]
struct ClientMetadata {
    id: String,
    secret: String,
    project: Option<String>,
}

pub(crate) struct GoogleOAuth {
    client: reqwest::Client,
    clients: BTreeMap<SubscriptionProvider, ClientMetadata>,
    token_url: String,
    userinfo_url: String,
    codeassist_url: String,
    daily_url: String,
    poll_delay: Duration,
}

impl GoogleOAuth {
    pub(crate) fn load(client: reqwest::Client, path: &Path) -> Self {
        let file: Value = std::fs::metadata(path)
            .ok()
            .filter(|metadata| metadata.len() < 65_536)
            .and_then(|_| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        let mut clients = BTreeMap::new();
        for (provider, default_id, prefix) in [
            (
                SubscriptionProvider::Antigravity,
                ANTIGRAVITY_CLIENT,
                "DOUANE_ANTIGRAVITY",
            ),
            (SubscriptionProvider::Gemini, GEMINI_CLIENT, "DOUANE_GEMINI"),
        ] {
            let config = file.get(provider.as_str()).unwrap_or(&Value::Null);
            let id = std::env::var(format!("{prefix}_CLIENT_ID"))
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| text(config, &["client_id"]))
                .unwrap_or_else(|| default_id.into());
            let secret = std::env::var(format!("{prefix}_CLIENT_SECRET"))
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| text(config, &["client_secret"]));
            if let Some(secret) = secret {
                clients.insert(
                    provider,
                    ClientMetadata {
                        id,
                        secret,
                        project: std::env::var(format!("{prefix}_PROJECT_ID"))
                            .ok()
                            .filter(|value| !value.trim().is_empty())
                            .or_else(|| text(config, &["project_id"])),
                    },
                );
            }
        }
        Self {
            client,
            clients,
            token_url: "https://oauth2.googleapis.com/token".into(),
            userinfo_url: "https://www.googleapis.com/oauth2/v2/userinfo?alt=json".into(),
            codeassist_url: "https://cloudcode-pa.googleapis.com/v1internal".into(),
            daily_url: "https://daily-cloudcode-pa.googleapis.com/v1internal".into(),
            poll_delay: Duration::from_secs(5),
        }
    }

    pub(crate) fn supports(&self, provider: SubscriptionProvider) -> bool {
        self.clients.contains_key(&provider)
    }

    pub(crate) fn authorize(
        &self,
        provider: SubscriptionProvider,
        state: &str,
        redirect: &str,
        challenge: &str,
    ) -> Result<String, SubscriptionError> {
        let metadata = self
            .clients
            .get(&provider)
            .ok_or(SubscriptionError::OAuthNotConfigured)?;
        let mut scopes = vec![
            "https://www.googleapis.com/auth/cloud-platform",
            "https://www.googleapis.com/auth/userinfo.email",
            "https://www.googleapis.com/auth/userinfo.profile",
        ];
        if provider == SubscriptionProvider::Antigravity {
            scopes.extend([
                "https://www.googleapis.com/auth/cclog",
                "https://www.googleapis.com/auth/experimentsandconfigs",
            ]);
        }
        let scopes = scopes.join(" ");
        let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
            .map_err(|_| SubscriptionError::InvalidResponse)?;
        url.query_pairs_mut().extend_pairs([
            ("client_id", metadata.id.as_str()),
            ("redirect_uri", redirect),
            ("response_type", "code"),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("scope", scopes.as_str()),
            ("state", state),
        ]);
        if provider == SubscriptionProvider::Gemini
            && redirect == "https://codeassist.google.com/authcode"
        {
            url.query_pairs_mut().extend_pairs([
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
            ]);
        }
        Ok(url.into())
    }

    pub(crate) async fn exchange(
        &self,
        provider: SubscriptionProvider,
        code: &str,
        redirect: &str,
        verifier: &str,
    ) -> Result<Value, SubscriptionError> {
        let metadata = self
            .clients
            .get(&provider)
            .ok_or(SubscriptionError::OAuthNotConfigured)?;
        let mut form = vec![
            ("client_id", metadata.id.as_str()),
            ("client_secret", metadata.secret.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
        ];
        if provider == SubscriptionProvider::Gemini
            && redirect == "https://codeassist.google.com/authcode"
        {
            form.push(("code_verifier", verifier));
        }
        let mut tokens = send_json(self.client.post(&self.token_url).form(&form)).await?;
        let access = text(&tokens, &["access_token"]).ok_or(SubscriptionError::InvalidResponse)?;
        let profile = send_json(self.client.get(&self.userinfo_url).bearer_auth(&access)).await?;
        let email = text(&profile, &["email"]).ok_or(SubscriptionError::InvalidResponse)?;
        let (project, tier) = self
            .onboard(provider, &access, metadata.project.as_deref())
            .await?;
        let expires = tokens
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|seconds| *seconds > 0)
            .map(|seconds| {
                timestamp(
                    time::OffsetDateTime::now_utc()
                        + time::Duration::seconds(seconds.min(31_536_000)),
                )
            });
        let object = tokens
            .as_object_mut()
            .ok_or(SubscriptionError::InvalidResponse)?;
        object.insert("type".into(), json!(provider.as_str()));
        object.insert("email".into(), json!(email));
        object.insert(
            "account_id".into(),
            profile.get("id").cloned().unwrap_or_else(|| json!(email)),
        );
        object.insert("project_id".into(), json!(project));
        object.insert("client_id".into(), json!(metadata.id));
        if let Some(tier) = tier {
            object.insert("plan".into(), json!(tier));
        }
        if let Some(expires) = expires {
            object.insert("expired".into(), json!(expires));
        }
        Ok(tokens)
    }

    pub(crate) fn refresh_request(
        &self,
        provider: SubscriptionProvider,
        refresh: &str,
        imported_id: Option<&str>,
        imported_secret: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, SubscriptionError> {
        let metadata = self.clients.get(&provider);
        let id = imported_id
            .or_else(|| metadata.map(|metadata| metadata.id.as_str()))
            .ok_or(SubscriptionError::OAuthNotConfigured)?;
        let secret = imported_secret
            .or_else(|| metadata.map(|metadata| metadata.secret.as_str()))
            .ok_or(SubscriptionError::OAuthNotConfigured)?;
        Ok(self.client.post(&self.token_url).form(&[
            ("client_id", id),
            ("client_secret", secret),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ]))
    }

    async fn onboard(
        &self,
        provider: SubscriptionProvider,
        access: &str,
        project: Option<&str>,
    ) -> Result<(String, Option<String>), SubscriptionError> {
        let metadata = if provider == SubscriptionProvider::Antigravity {
            json!({"ideType":"ANTIGRAVITY"})
        } else {
            json!({"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"})
        };
        let mut body = json!({"metadata":metadata});
        if let Some(project) = project {
            body["cloudaicompanionProject"] = json!(project);
        }
        let mut request = self
            .client
            .post(format!("{}:loadCodeAssist", self.codeassist_url))
            .bearer_auth(access)
            .json(&body);
        if provider == SubscriptionProvider::Antigravity {
            request = request.header("User-Agent", antigravity_user_agent());
        } else {
            request = request.header("User-Agent", crate::relay::GEMINI_USER_AGENT);
        }
        let loaded = send_json(request).await?;
        if let Some(error) = load_verification_error(&loaded) {
            return Err(error);
        }
        let tier = loaded
            .get("paidTier")
            .and_then(|tier| text(tier, &["id"]))
            .or_else(|| {
                loaded
                    .get("currentTier")
                    .and_then(|tier| text(tier, &["id"]))
            });
        if let Some(project) = project_id(&loaded) {
            return Ok((project, tier));
        }
        if provider == SubscriptionProvider::Gemini
            && loaded.get("currentTier").is_some_and(Value::is_object)
        {
            return project.map(|project| (project.to_owned(), tier)).ok_or(
                SubscriptionError::InvalidImport(
                    "Google account requires a configured Cloud project",
                ),
            );
        }
        // Subscription labels are separate from the activation tier sent upstream.
        let selected = loaded
            .get("allowedTiers")
            .and_then(Value::as_array)
            .and_then(|tiers| tiers.iter().find(|tier| tier["isDefault"] == true))
            .and_then(|tier| text(tier, &["id"]))
            .or_else(|| {
                if provider == SubscriptionProvider::Antigravity {
                    loaded
                        .get("currentTier")
                        .and_then(|tier| text(tier, &["id"]))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                if provider == SubscriptionProvider::Antigravity {
                    "free-tier".into()
                } else {
                    "legacy-tier".into()
                }
            });
        if provider == SubscriptionProvider::Gemini && selected != "free-tier" && project.is_none()
        {
            return Err(SubscriptionError::InvalidImport(
                "Google account requires a configured Cloud project",
            ));
        }
        let (endpoint, body) = if provider == SubscriptionProvider::Antigravity {
            (
                format!("{}:onboardUser", self.daily_url),
                json!({"tier_id":selected,"metadata":{"ide_type":"ANTIGRAVITY","ide_name":"antigravity","ide_version":ANTIGRAVITY_VERSION}}),
            )
        } else {
            let mut body = json!({"tierId":selected,"metadata":metadata});
            if selected != "free-tier" {
                body["cloudaicompanionProject"] = json!(project);
                body["metadata"]["duetProject"] = json!(project);
            }
            (format!("{}:onboardUser", self.codeassist_url), body)
        };
        let onboard_request = || {
            let mut request = self.client.post(&endpoint).bearer_auth(access).json(&body);
            if provider == SubscriptionProvider::Antigravity {
                request = request
                    .header("User-Agent", antigravity_user_agent())
                    .header("X-Goog-Api-Client", "gl-node/22.21.1");
            } else {
                request = request.header("User-Agent", crate::relay::GEMINI_USER_AGENT);
            }
            request
        };
        let mut operation = send_json(onboard_request()).await?;
        for attempt in 0..12 {
            if operation["done"] == true {
                if let Some(error) = completed_onboarding_error(&operation, &endpoint) {
                    return Err(error);
                }
                let Some(project) = operation.get("response").and_then(project_id) else {
                    // Never log the operation, profile, project, or upstream error text.
                    tracing::warn!(
                        provider = provider.as_str(),
                        activation_tier = activation_tier_category(&selected),
                        current_tier_present = loaded
                            .get("currentTier")
                            .is_some_and(serde_json::Value::is_object),
                        paid_tier_present = loaded
                            .get("paidTier")
                            .is_some_and(serde_json::Value::is_object),
                        allowed_tier_count = loaded
                            .get("allowedTiers")
                            .and_then(serde_json::Value::as_array)
                            .map_or(0, Vec::len),
                        ineligible_tier_count = loaded
                            .get("ineligibleTiers")
                            .and_then(serde_json::Value::as_array)
                            .map_or(0, Vec::len),
                        ineligible_reasons = ?ineligible_reason_categories(&loaded),
                        onboarding_status = diagnostic_enum(
                            operation.pointer("/response/status/statusCode"),
                            &["DEFAULT", "NOTICE", "WARNING"]
                        ),
                        help_link_present = operation.pointer("/response/status/helpLink/url").is_some(),
                        response_present = operation
                            .get("response")
                            .is_some_and(serde_json::Value::is_object),
                        "Google onboarding completed without a Cloud project"
                    );
                    return Err(SubscriptionError::GoogleProjectUnavailable);
                };
                return Ok((project, tier.or(Some(selected))));
            }
            if attempt == 11 {
                break;
            }
            tokio::time::sleep(self.poll_delay).await;
            if provider == SubscriptionProvider::Antigravity {
                operation = send_json(onboard_request()).await?;
            } else {
                let name = text(&operation, &["name"]).ok_or(SubscriptionError::InvalidResponse)?;
                if !name.starts_with("operations/")
                    || name.contains("..")
                    || name.contains('?')
                    || name.contains('#')
                {
                    return Err(SubscriptionError::InvalidResponse);
                }
                operation = send_json(
                    self.client
                        .get(format!("{}/{name}", self.codeassist_url))
                        .bearer_auth(access),
                )
                .await?;
            }
        }
        Err(SubscriptionError::InvalidImport(
            "Google account onboarding did not finish",
        ))
    }
}

fn activation_tier_category(tier: &str) -> &'static str {
    match tier {
        "free-tier" => "free-tier",
        "standard-tier" => "standard-tier",
        "legacy-tier" => "legacy-tier",
        _ => "other",
    }
}

fn ineligible_reason_categories(loaded: &Value) -> Vec<&'static str> {
    const REASONS: &[&str] = &[
        "UNKNOWN",
        "INELIGIBLE_ACCOUNT",
        "UNKNOWN_LOCATION",
        "UNSUPPORTED_LOCATION",
        "RESTRICTED_NETWORK",
        "RESTRICTED_AGE",
        "NON_USER_ACCOUNT",
        "DASHER_USER",
        "BYOID_USER",
        "RESTRICTED_DASHER_USER",
        "VALIDATION_REQUIRED",
        "GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT",
        "UNSUPPORTED_CLIENT",
    ];
    loaded
        .get("ineligibleTiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(16)
        .map(|tier| diagnostic_enum(tier.get("reasonCode"), REASONS))
        .collect()
}

fn diagnostic_enum(value: Option<&Value>, known: &'static [&'static str]) -> &'static str {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return "absent";
    };
    if let Some(code) = value.as_str() {
        known
            .iter()
            .copied()
            .find(|&known| known == code)
            .unwrap_or("other")
    } else {
        value
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| known.get(index).copied())
            .unwrap_or("other")
    }
}

fn load_verification_error(value: &Value) -> Option<SubscriptionError> {
    if value.get("currentTier").is_some_and(Value::is_object) {
        return None;
    }
    let blocked = value
        .get("ineligibleTiers")
        .and_then(Value::as_array)?
        .iter()
        .find(|tier| tier["reasonCode"] == "VALIDATION_REQUIRED")?;
    Some(SubscriptionError::verification_required(
        blocked.get("validationUrl").and_then(Value::as_str),
    ))
}

fn completed_onboarding_error(value: &Value, endpoint: &str) -> Option<SubscriptionError> {
    let error = value.get("error").filter(|error| !error.is_null())?;
    // Long-running operations use google.rpc.Status (7 = PERMISSION_DENIED).
    // Some HTTP adapters retain its corresponding HTTP status code instead.
    if matches!(error.get("code").and_then(Value::as_u64), Some(7 | 403)) {
        if let (Ok(source), Ok(body)) = (
            reqwest::Url::parse(endpoint),
            serde_json::to_vec(&json!({"error": error})),
        ) {
            if let Some(error) =
                google_verification_error(reqwest::StatusCode::FORBIDDEN, &source, &body)
            {
                return Some(error);
            }
        }
    }
    Some(SubscriptionError::InvalidImport(
        "Google account onboarding failed",
    ))
}

fn project_id(value: &Value) -> Option<String> {
    ["cloudaicompanionProject", "projectId", "project"]
        .into_iter()
        .find_map(|key| {
            value.get(key).and_then(|project| {
                project
                    .as_str()
                    .filter(|project| !project.trim().is_empty())
                    .map(str::to_owned)
                    .or_else(|| text(project, &["id"]))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn onboarding_diagnostics_only_contain_known_categories() {
        let loaded = json!({"ineligibleTiers": [
            {"reasonCode":"VALIDATION_REQUIRED","validationUrl":"https://accounts.google.com/signin/continue?private=value"},
            {"reasonCode":"GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT"},
            {"reasonCode":"UNSUPPORTED_CLIENT"},
            {"reasonCode":"private-provider-value"},
            {"reasonMessage":"private-provider-message"}
        ]});
        assert_eq!(
            ineligible_reason_categories(&loaded),
            vec![
                "VALIDATION_REQUIRED",
                "GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT",
                "UNSUPPORTED_CLIENT",
                "other",
                "absent"
            ]
        );
        assert_eq!(activation_tier_category("private-plan"), "other");
        assert_eq!(
            diagnostic_enum(Some(&json!(2)), &["DEFAULT", "NOTICE", "WARNING"]),
            "WARNING"
        );
        assert_eq!(
            diagnostic_enum(Some(&json!(999)), &["DEFAULT", "NOTICE", "WARNING"]),
            "other"
        );
        assert_eq!(
            diagnostic_enum(
                Some(&json!("private-status")),
                &["DEFAULT", "NOTICE", "WARNING"]
            ),
            "other"
        );
    }

    #[tokio::test]
    async fn google_code_exchange_profiles_and_onboards_a_managed_project() {
        let polls = Arc::new(AtomicUsize::new(0));
        let worker_polls = polls.clone();
        let app = axum::Router::new()
            .route("/token",axum::routing::post(|axum::Form(form): axum::Form<BTreeMap<String,String>>|async move{
                assert_eq!(form["grant_type"],"authorization_code");assert_eq!(form["client_id"],"fixture-client");
                assert_eq!(form["redirect_uri"],"https://codeassist.google.com/authcode");assert_eq!(form["code_verifier"],"fixture-verifier");
                axum::Json(json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_in":3600}))}))
            .route("/userinfo",axum::routing::get(||async{axum::Json(json!({"email":"fixture@example.test","id":"google-subject"}))}))
            .route("/v1internal:loadCodeAssist",axum::routing::post(||async{axum::Json(json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]}))}))
            .route("/v1internal:onboardUser",axum::routing::post(|axum::Json(body):axum::Json<Value>|async move{
                assert_eq!(body["tierId"],"free-tier");assert!(body.get("cloudaicompanionProject").is_none());
                assert_eq!(body["metadata"]["pluginType"],"GEMINI");
                axum::Json(json!({"name":"operations/fixture","done":false}))}))
            .route("/v1internal/operations/fixture",axum::routing::get(move||{let polls=worker_polls.clone();async move{
                polls.fetch_add(1,Ordering::SeqCst);axum::Json(json!({"done":true,"response":{"cloudaicompanionProject":{"id":"managed-project"}}}))
            }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.clients.insert(
            SubscriptionProvider::Gemini,
            ClientMetadata {
                id: "fixture-client".into(),
                secret: "fixture-public-client-metadata".into(),
                project: None,
            },
        );
        google.token_url = format!("{endpoint}/token");
        google.userinfo_url = format!("{endpoint}/userinfo");
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.poll_delay = Duration::ZERO;
        let value = google
            .exchange(
                SubscriptionProvider::Gemini,
                "fixture-code",
                "https://codeassist.google.com/authcode",
                "fixture-verifier",
            )
            .await
            .unwrap();
        assert_eq!(value["project_id"], "managed-project");
        assert_eq!(value["email"], "fixture@example.test");
        assert_eq!(value["type"], "gemini");
        assert!(value.get("client_secret").is_none());
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn antigravity_onboards_and_refreshes_with_runtime_client_metadata() {
        let polls = Arc::new(AtomicUsize::new(0));
        let worker_polls = polls.clone();
        let app = axum::Router::new()
            .route("/token", axum::routing::post(|axum::Form(form):axum::Form<BTreeMap<String,String>>|async move {
                assert_eq!(form["grant_type"],"refresh_token");assert_eq!(form["client_id"],"fixture-antigravity");
                assert_eq!(form["refresh_token"],"fixture-refresh");
                axum::Json(json!({"access_token":"fixture-rotated","expires_in":3600}))
            }))
            .route("/v1internal:loadCodeAssist",axum::routing::post(|headers:axum::http::HeaderMap, axum::Json(body):axum::Json<Value>|async move {
                assert_eq!(body["metadata"]["ideType"],"ANTIGRAVITY");
                assert_eq!(headers["user-agent"], antigravity_user_agent());
                axum::Json(json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]}))
            }))
            .route("/daily:onboardUser",axum::routing::post(move|headers:axum::http::HeaderMap, axum::Json(body):axum::Json<Value>|{
                let polls=worker_polls.clone();async move {
                    assert_eq!(body["tier_id"],"free-tier");assert_eq!(body["metadata"]["ide_type"],"ANTIGRAVITY");
                    assert_eq!(body["metadata"]["ide_version"], ANTIGRAVITY_VERSION);
                    assert_eq!(headers["user-agent"], antigravity_user_agent());
                    assert_eq!(headers["x-goog-api-client"],"gl-node/22.21.1");
                    let ready = polls.fetch_add(1,Ordering::SeqCst) > 0;
                    axum::Json(if ready {json!({"done":true,"response":{"cloudaicompanionProject":"antigravity-project"}})}else{json!({"done":false})})
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.clients.insert(
            SubscriptionProvider::Antigravity,
            ClientMetadata {
                id: "fixture-antigravity".into(),
                secret: "fixture-public-metadata".into(),
                project: None,
            },
        );
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.daily_url = format!("{endpoint}/daily");
        google.token_url = format!("{endpoint}/token");
        google.poll_delay = Duration::ZERO;
        let (project, tier) = google
            .onboard(SubscriptionProvider::Antigravity, "fixture-access", None)
            .await
            .unwrap();
        assert_eq!(project, "antigravity-project");
        assert_eq!(tier.as_deref(), Some("free-tier"));
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        let tokens = send_json(
            google
                .refresh_request(
                    SubscriptionProvider::Antigravity,
                    "fixture-refresh",
                    Some("fixture-antigravity"),
                    None,
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(tokens["access_token"], "fixture-rotated");
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn antigravity_onboarding_separates_activation_tier_from_paid_plan() {
        let loaded = Arc::new(std::sync::Mutex::new(Value::Null));
        let worker_loaded = loaded.clone();
        let requested = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let worker_requested = requested.clone();
        let app = axum::Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                axum::routing::post(move || {
                    let loaded = worker_loaded.clone();
                    async move { axum::Json(loaded.lock().unwrap().clone()) }
                }),
            )
            .route(
                "/daily:onboardUser",
                axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                    let requested = worker_requested.clone();
                    async move {
                        requested
                            .lock()
                            .unwrap()
                            .push(body["tier_id"].as_str().unwrap().to_owned());
                        axum::Json(json!({
                            "done": true,
                            "response": {"cloudaicompanionProject": {"id": "fixture-project"}}
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.daily_url = format!("{endpoint}/daily");
        google.poll_delay = Duration::ZERO;

        for (response, expected_tier) in [
            (
                json!({"currentTier":{"id":"standard-tier"},"paidTier":{"id":"g1-pro-tier"}}),
                "standard-tier",
            ),
            (json!({"paidTier":{"id":"g1-pro-tier"}}), "free-tier"),
            (
                json!({"currentTier":{"id":"standard-tier"},"paidTier":{"id":"g1-pro-tier"},"allowedTiers":[{"id":"free-tier","isDefault":true}]}),
                "free-tier",
            ),
        ] {
            *loaded.lock().unwrap() = response;
            let (project, plan) = google
                .onboard(SubscriptionProvider::Antigravity, "fixture-access", None)
                .await
                .unwrap();
            assert_eq!(requested.lock().unwrap().last().unwrap(), expected_tier);
            assert_eq!(project, "fixture-project");
            assert_eq!(plan.as_deref(), Some("g1-pro-tier"));
        }
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn completed_onboarding_without_a_project_is_an_activation_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let app = axum::Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                axum::routing::post(|| async {
                    axum::Json(json!({"currentTier": {"id": "standard-tier"}}))
                }),
            )
            .route(
                "/daily:onboardUser",
                axum::routing::post(move || {
                    let calls = worker_calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({"done": true, "response": {"private": "fixture-secret"}}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.daily_url = format!("{endpoint}/daily");
        google.poll_delay = Duration::ZERO;

        let error = google
            .onboard(SubscriptionProvider::Antigravity, "fixture-access", None)
            .await
            .unwrap_err();
        task.abort();
        let _ = task.await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            error.to_string(),
            "Google did not return a Cloud project for this account; check account setup in Antigravity"
        );
        assert!(!format!("{error:?}").contains("fixture-secret"));
    }

    #[tokio::test]
    async fn google_load_verification_stops_before_onboarding_without_exposing_provider_data() {
        let verification_url =
            "https://accounts.google.com/signin/continue?fixture=verification-challenge";
        let loaded = Arc::new(std::sync::Mutex::new(Value::Null));
        let worker_loaded = loaded.clone();
        let onboard_calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = onboard_calls.clone();
        let app = axum::Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                axum::routing::post(move || {
                    let loaded = worker_loaded.clone();
                    async move { axum::Json(loaded.lock().unwrap().clone()) }
                }),
            )
            .route(
                "/daily:onboardUser",
                axum::routing::post(move || {
                    let calls = worker_calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({
                            "done": true,
                            "response": {"cloudaicompanionProject": "fixture-project"}
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.daily_url = format!("{endpoint}/daily");
        google.poll_delay = Duration::ZERO;

        for provider in [
            SubscriptionProvider::Antigravity,
            SubscriptionProvider::Gemini,
        ] {
            for url in [
                Some(verification_url),
                Some("https://evil.example/challenge"),
                None,
            ] {
                *loaded.lock().unwrap() = json!({
                    "currentTier": null,
                    "ineligibleTiers": [{
                        "reasonCode": "VALIDATION_REQUIRED",
                        "reasonMessage": "provider-body-private",
                        "validationUrl": url
                    }]
                });
                let error = google
                    .onboard(provider, "fixture-access", None)
                    .await
                    .unwrap_err();
                assert!(error.is_verification_required());
                assert_eq!(
                    error.verification_url(),
                    url.filter(|url| *url == verification_url)
                );
                for rendered in [error.to_string(), format!("{error:?}")] {
                    assert!(!rendered.contains("provider-body-private"));
                    assert!(!rendered.contains("verification-challenge"));
                    assert!(!rendered.contains("evil.example"));
                }
            }
        }
        assert_eq!(onboard_calls.load(Ordering::SeqCst), 0);

        // Eligibility for one tier can coexist with a verification gate for another.
        // Keep the current eligible account connected, as Google's setup does.
        for paid_tier in [Value::Null, json!({})] {
            *loaded.lock().unwrap() = json!({
                "currentTier": {"id": "standard-tier"},
                "paidTier": paid_tier,
                "cloudaicompanionProject": "fixture-existing-project",
                "ineligibleTiers": [{
                    "reasonCode": "VALIDATION_REQUIRED",
                    "validationUrl": verification_url
                }]
            });
            let result = google
                .onboard(SubscriptionProvider::Antigravity, "fixture-access", None)
                .await
                .unwrap();
            assert_eq!(
                result,
                (
                    "fixture-existing-project".into(),
                    Some("standard-tier".into())
                )
            );
        }
        assert_eq!(onboard_calls.load(Ordering::SeqCst), 0);
        task.abort();
        let _ = task.await;
    }

    #[test]
    fn google_completed_onboarding_classifies_only_trusted_permission_verification() {
        let source = "https://daily-cloudcode-pa.googleapis.com/v1internal:onboardUser";
        let verification_url = "https://accounts.google.com/signin/continue?fixture=challenge";
        for code in [7, 403] {
            let operation = json!({
                "done": true,
                "error": {
                    "code": code,
                    "message": "provider-body-private",
                    "details": [{
                        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                        "reason": "VALIDATION_REQUIRED",
                        "domain": "cloudcode-pa.googleapis.com",
                        "metadata": {"validation_link": verification_url}
                    }]
                }
            });
            let error = completed_onboarding_error(&operation, source).unwrap();
            assert!(error.is_verification_required());
            assert_eq!(error.verification_url(), Some(verification_url));
            assert!(!error.to_string().contains("provider-body-private"));
            assert!(!format!("{error:?}").contains("fixture=challenge"));

            let error =
                completed_onboarding_error(&operation, "https://evil.example/onboardUser").unwrap();
            assert!(matches!(
                error,
                SubscriptionError::InvalidImport("Google account onboarding failed")
            ));
            let mut non_permission = operation;
            non_permission["error"]["code"] = json!(13);
            let error = completed_onboarding_error(&non_permission, source).unwrap();
            assert!(matches!(
                error,
                SubscriptionError::InvalidImport("Google account onboarding failed")
            ));
        }
        assert!(
            completed_onboarding_error(&json!({"done": true, "response": {}}), source).is_none()
        );
    }

    #[test]
    fn no_config_means_no_advertised_google_login_and_oob_has_pkce() {
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.clients.clear();
        assert!(!google.supports(SubscriptionProvider::Antigravity));
        google.clients.insert(
            SubscriptionProvider::Gemini,
            ClientMetadata {
                id: "fixture-client".into(),
                secret: "fixture-metadata".into(),
                project: None,
            },
        );
        let url = google
            .authorize(
                SubscriptionProvider::Gemini,
                "state",
                "https://codeassist.google.com/authcode",
                "challenge",
            )
            .unwrap();
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!url.contains("fixture-metadata"));
    }
}
