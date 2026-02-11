//! Types for the OAuth2 client module.

use crate::Authentication;
use std::time::{SystemTime, UNIX_EPOCH};

/// Tokens obtained from an OAuth2/OIDC flow.
#[derive(Clone, Debug)]
pub struct OAuthTokens {
    /// The access token for Bearer authentication.
    pub access_token: String,
    /// The refresh token for obtaining new access tokens.
    pub refresh_token: Option<String>,
    /// Unix timestamp (seconds) when the access token expires.
    pub expires_at: Option<u64>,
    /// The token endpoint URL (used for refresh).
    pub token_url: String,
    /// The OAuth2 client ID.
    pub client_id: String,
}

impl OAuthTokens {
    /// Convert into an [`Authentication::OAuth2Token`] for storage.
    pub fn into_authentication(self) -> Authentication {
        Authentication::OAuth2Token {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            token_url: self.token_url,
            client_id: self.client_id,
            expires_at: self.expires_at,
        }
    }

    /// Returns `true` if the access token is expired or will expire within 30
    /// seconds.
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system time before UNIX epoch")
                    .as_secs();
                now + 30 >= exp
            }
            // No expiry information; assume not expired.
            None => false,
        }
    }
}

/// Errors that can occur during OAuth2 operations.
#[derive(Debug, thiserror::Error)]
pub enum OAuth2Error {
    /// OIDC discovery failed.
    #[error("OIDC discovery failed: {0}")]
    Discovery(String),

    /// The authorization server did not return a token endpoint.
    #[error("provider metadata does not include a token endpoint")]
    MissingTokenEndpoint,

    /// Failed to exchange authorization code or device code for tokens.
    #[error("token exchange failed: {0}")]
    TokenExchange(String),

    /// Failed to refresh an access token.
    #[error("token refresh failed: {0}")]
    TokenRefresh(String),

    /// Could not open the browser for the authorization URL.
    #[error("failed to open browser: {0}")]
    BrowserOpen(String),

    /// The local callback server failed.
    #[error("callback server error: {0}")]
    CallbackServer(String),

    /// The state parameter returned by the server did not match.
    #[error("CSRF state mismatch")]
    StateMismatch,

    /// The authorization server returned an error during device code polling.
    #[error("device authorization failed: {0}")]
    DeviceAuthorization(String),

    /// A reqwest HTTP error.
    #[error(transparent)]
    Http(#[from] reqwest::Error),

    /// A URL parse error.
    #[error(transparent)]
    UrlParse(#[from] url::ParseError),
}
