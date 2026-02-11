//! OAuth2/OIDC token refresh support.
//!
//! This module provides [`refresh_token`] to refresh an existing access token
//! using a stored refresh token, token URL, and client ID. The interactive
//! authentication flows (authorization code + PKCE and device code) live in the
//! `rattler` CLI crate.

pub mod types;

pub use types::{OAuth2Error, OAuthTokens};

use openidconnect::{
    core::{CoreClient, CoreProviderMetadata},
    ClientId, IssuerUrl, OAuth2TokenResponse, RefreshToken,
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Refresh an OAuth2 access token using a stored refresh token.
///
/// This builds a minimal OIDC client from the stored `token_url` and
/// `client_id`, then exchanges the refresh token for a new access token.
pub async fn refresh_token(
    http_client: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    refresh_token_value: &str,
) -> Result<OAuthTokens, OAuth2Error> {
    // We need an issuer URL for the CoreClient; derive it from the token URL.
    // The token URL is usually `https://issuer/oauth/token` or similar.
    let token_url_parsed =
        url::Url::parse(token_url).map_err(|e| OAuth2Error::TokenRefresh(e.to_string()))?;
    let issuer_str = format!(
        "{}://{}",
        token_url_parsed.scheme(),
        token_url_parsed
            .host_str()
            .ok_or_else(|| OAuth2Error::TokenRefresh("token URL has no host".to_string()))?
    );
    let issuer = IssuerUrl::new(issuer_str)
        .map_err(|e| OAuth2Error::TokenRefresh(format!("invalid issuer URL: {e}")))?;

    // Discover provider metadata (this lets us build a client with the correct
    // token endpoint, JWKS, etc.)
    let provider_metadata = CoreProviderMetadata::discover_async(issuer, http_client)
        .await
        .map_err(|e| OAuth2Error::TokenRefresh(format!("discovery failed: {e}")))?;

    let oidc_client = CoreClient::from_provider_metadata(
        provider_metadata,
        ClientId::new(client_id.to_string()),
        None,
    );

    let refresh = RefreshToken::new(refresh_token_value.to_string());
    let token_response = oidc_client
        .exchange_refresh_token(&refresh)
        .map_err(|e| OAuth2Error::TokenRefresh(e.to_string()))?
        .request_async(http_client)
        .await
        .map_err(|e| OAuth2Error::TokenRefresh(e.to_string()))?;

    let access_token = token_response.access_token().secret().clone();
    let new_refresh = token_response
        .refresh_token()
        .map(|t| t.secret().clone())
        .unwrap_or_else(|| refresh_token_value.to_string());
    let expires_at = token_response.expires_in().map(|d| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before UNIX epoch")
            .as_secs()
            + d.as_secs()
    });

    Ok(OAuthTokens {
        access_token,
        refresh_token: Some(new_refresh),
        expires_at,
        token_url: token_url.to_string(),
        client_id: client_id.to_string(),
    })
}
