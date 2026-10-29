//! Subscription credentials, OAuth sessions, and provider-reported quotas.
//!
//! Credentials are persisted locally, while public account responses use an
//! explicit allowlist. Provider failures never copy response bodies into errors.

mod forecast;
mod google;
pub(crate) mod meta;
mod oauth;
mod quota;
mod reset;
mod store;

pub use oauth::{OAuthAttempt, OAuthManager};
pub use quota::{
    QuotaCredits, QuotaService, QuotaSnapshot, QuotaWindow, RESERVE_WINDOWS, reserve_kind,
};
pub(crate) use quota::{window_seconds, write_private};
pub use reset::{ResetError, ResetManager, ResetOutcome, ResetRedemption, ResetStatus};
pub use store::{
    AccountCredential, AccountStore, AccountSummary, EndpointPatch, SubscriptionProvider,
    SubscriptionTier, reserve_windows, valid_base_url, valid_dialect, valid_reserve, valid_route,
};

pub struct GoogleVerification {
    url: Option<String>,
}

impl std::fmt::Debug for GoogleVerification {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleVerification")
            .field("verification_available", &self.url.is_some())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubscriptionError {
    #[error("invalid authentication file: {0}")]
    InvalidImport(&'static str),
    #[error("account not found")]
    NotFound,
    #[error("provider does not support this operation")]
    Unsupported,
    #[error("authentication storage failed")]
    Storage,
    #[error("{}", upstream_message(.0))]
    Upstream(u16),
    #[error("Google account verification is required; complete verification and retry")]
    VerificationRequired(GoogleVerification),
    #[error(
        "Google did not return a Cloud project for this account; check account setup in Antigravity"
    )]
    GoogleProjectUnavailable,
    #[error("upstream authentication or quota request could not be completed")]
    Transport,
    #[error("upstream returned an invalid response")]
    InvalidResponse,
    #[error("OAuth attempt expired or was already consumed")]
    Expired,
    #[error("OAuth state does not match")]
    InvalidState,
    #[error("OAuth redirect URI is not a permitted callback")]
    InvalidRedirect,
    #[error("account has no refresh token")]
    NoRefreshToken,
    #[error("the provider revoked this account's session; reconnect the account")]
    SessionRevoked,
    #[error("provider OAuth client is not configured")]
    OAuthNotConfigured,
    #[error("reserve_percent must be greater than 0 and less than 100, or null/0 to clear")]
    InvalidReserve,
    #[error(
        "reserve_windows accepts five-hour, daily, weekly and monthly; empty watches the longest window"
    )]
    InvalidReserveWindows,
    #[error("{0}")]
    InvalidEndpoint(&'static str),
}

/// A 429 is the provider throttling this account's reads, not a login fault.
fn upstream_message(status: &u16) -> String {
    match status {
        429 => {
            "provider is rate-limiting this account's quota reads (HTTP 429); retry later".into()
        }
        status => format!("upstream authentication or quota request failed (HTTP {status})"),
    }
}

impl SubscriptionError {
    pub(crate) fn verification_required(url: Option<&str>) -> Self {
        Self::VerificationRequired(GoogleVerification {
            url: url.and_then(oauth::validate_google_verification_url),
        })
    }

    pub(crate) fn is_verification_required(&self) -> bool {
        matches!(self, Self::VerificationRequired(_))
    }

    /// A failure that can pass on its own: rate limiting, a provider outage,
    /// a lost connection or a garbled answer. Auth and setup failures are not.
    pub(crate) fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Transport | Self::InvalidResponse | Self::Upstream(408 | 425 | 429 | 500..=599)
        )
    }

    pub(crate) fn verification_url(&self) -> Option<&str> {
        match self {
            Self::VerificationRequired(verification) => verification.url.as_deref(),
            _ => None,
        }
    }
}

pub(crate) fn now() -> String {
    timestamp(time::OffsetDateTime::now_utc())
}

pub(crate) fn timestamp(value: time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.unix_timestamp().to_string())
}

pub(crate) fn parse_timestamp(value: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
}

pub(crate) fn random_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
