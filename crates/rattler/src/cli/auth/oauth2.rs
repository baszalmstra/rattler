//! OAuth2/OIDC interactive authentication flows for the CLI.
//!
//! This module contains the Authorization Code + PKCE and Device Code flows,
//! which were moved here from `rattler_networking` because they are interactive
//! CLI behaviors (opening browsers, printing codes, running local servers).

use openidconnect::AuthType;
use rattler_networking::oauth2_client::{OAuth2Error, OAuthTokens};

use super::AuthenticationCLIError;

/// The default OAuth2 client ID for prefix.dev.
const PREFIX_DEV_CLIENT_ID: &str = "rattler";

/// Determine whether OAuth2 should be used for the given login args.
///
/// Returns `true` if:
/// - `--oauth2` flag is explicitly set, OR
/// - the host is a prefix.dev host and no other explicit auth method is given
pub(super) fn should_use_oauth2(args: &super::LoginArgs) -> bool {
    args.oauth2
}

/// Resolve the OIDC issuer URL for a given host.
pub(super) fn resolve_issuer_url(
    host: &str,
    explicit_issuer: Option<&str>,
) -> Result<String, AuthenticationCLIError> {
    if let Some(url) = explicit_issuer {
        return Ok(url.to_string());
    }

    // Derive from the host
    let host_clean = host.replace("*.", "");
    let host_clean = host_clean.strip_prefix("repo.").unwrap_or(&host_clean);
    if host_clean.contains("://") {
        Ok(host_clean.to_string())
    } else {
        Ok(format!("https://{host_clean}"))
    }
}

/// Resolve the OAuth2 client ID for a given host.
pub(super) fn resolve_client_id(explicit_client_id: Option<&str>) -> String {
    explicit_client_id
        .unwrap_or(PREFIX_DEV_CLIENT_ID)
        .to_string()
}

/// Run the OAuth2 login flow: try auth code first, fall back to device code.
pub(super) async fn run_oauth2_flow(
    issuer_url: &str,
    client_id: &str,
) -> Result<OAuthTokens, AuthenticationCLIError> {
    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(OAuth2Error::Http)?;

    eprintln!("Starting OAuth2 login flow...");

    // // Try Authorization Code + PKCE flow first (opens browser)
    // match authorization_code_flow(&http_client, issuer_url, client_id).await {
    //     Ok(tokens) => {
    //         eprintln!("Authentication successful via browser.");
    //         return Ok(tokens);
    //     }
    //     Err(OAuth2Error::BrowserOpen(e)) => {
    //         eprintln!("Could not open browser ({e}), falling back to device code flow...");
    //     }
    //     Err(e) => return Err(e.into()),
    // }

    // Fall back to Device Code flow
    let tokens = device_code_flow(&http_client, issuer_url, client_id).await?;
    eprintln!("Authentication successful via device code.");
    Ok(tokens)
}

/// Perform the Authorization Code + PKCE flow.
///
/// 1. Discovers the OIDC provider metadata from the issuer URL.
/// 2. Builds an authorization URL with PKCE.
/// 3. Starts a local callback server on `127.0.0.1` (random port).
/// 4. Opens the browser; returns an error if the browser cannot be opened (the
///    caller should fall back to the device code flow).
/// 5. Waits for the redirect callback, validates the state, and exchanges the
///    authorization code for tokens.
async fn authorization_code_flow(
    http_client: &reqwest::Client,
    issuer_url: &str,
    client_id: &str,
) -> Result<OAuthTokens, OAuth2Error> {
    use std::time::{SystemTime, UNIX_EPOCH};

    use openidconnect::{
        core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
        AuthorizationCode, ClientId, CsrfToken, IssuerUrl, Nonce, OAuth2TokenResponse,
        PkceCodeChallenge, RedirectUrl, Scope,
    };

    // 1. Discover provider metadata
    let issuer = IssuerUrl::new(issuer_url.to_string())
        .map_err(|e| OAuth2Error::Discovery(format!("invalid issuer URL '{issuer_url}': {e}")))?;

    let provider_metadata = CoreProviderMetadata::discover_async(issuer, http_client)
        .await
        .map_err(|e| OAuth2Error::Discovery(e.to_string()))?;

    let token_endpoint = provider_metadata
        .token_endpoint()
        .ok_or(OAuth2Error::MissingTokenEndpoint)?
        .to_string();

    // 2. Set up a one-shot channel to receive the callback
    let (tx, rx) = tokio::sync::oneshot::channel::<(AuthorizationCode, CsrfToken)>();

    // 3. Start a local callback server on a random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| OAuth2Error::CallbackServer(e.to_string()))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| OAuth2Error::CallbackServer(e.to_string()))?;
    let redirect_url = format!("http://127.0.0.1:{}/callback", local_addr.port());

    // 4. Build the OIDC client
    let oidc_client = CoreClient::from_provider_metadata(
        provider_metadata,
        ClientId::new(client_id.to_string()),
        None,
    )
    .set_redirect_uri(
        RedirectUrl::new(redirect_url.clone())
            .map_err(|e| OAuth2Error::CallbackServer(e.to_string()))?,
    );

    // 5. Generate PKCE challenge + authorization URL
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf_state, _nonce) = oidc_client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("offline_access".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    // 6. Build the axum callback handler
    let tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));
    let app = {
        let tx = tx.clone();
        axum::Router::new().route(
            "/callback",
            axum::routing::get(
                move |axum::extract::Query(params): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| {
                    let tx = tx.clone();
                    async move {
                        let code = params.get("code").cloned().unwrap_or_default();
                        let state = params.get("state").cloned().unwrap_or_default();
                        if let Some(sender) = tx.lock().await.take() {
                            let _ =
                                sender.send((AuthorizationCode::new(code), CsrfToken::new(state)));
                        }
                        axum::response::Html(
                            "<html><body><h1>Authentication successful!</h1>\
                             <p>You can close this tab and return to the terminal.</p>\
                             </body></html>"
                                .to_string(),
                        )
                    }
                },
            ),
        )
    };

    // 7. Open the browser
    open::that(auth_url.as_str()).map_err(|e| OAuth2Error::BrowserOpen(e.to_string()))?;
    eprintln!("Opened browser at:\n\n  {auth_url}\n");

    // 8. Serve until we get the callback (with a timeout)
    let server = axum::serve(listener, app);
    let callback_result = tokio::select! {
        result = rx => {
            result.map_err(|_| OAuth2Error::CallbackServer("callback channel closed".to_string()))
        }
        _ = async {
            // Run the server; it will be cancelled when the other branch completes
            let _ = server.await;
        } => {
            Err(OAuth2Error::CallbackServer("server stopped before callback".to_string()))
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            Err(OAuth2Error::CallbackServer("timed out waiting for callback (5 minutes)".to_string()))
        }
    };
    let (code, returned_state) = callback_result?;

    // 9. Validate state
    if returned_state.secret() != csrf_state.secret() {
        return Err(OAuth2Error::StateMismatch);
    }

    // 10. Exchange the authorization code for tokens
    let token_response = oidc_client
        .exchange_code(code)
        .map_err(|e| OAuth2Error::TokenExchange(e.to_string()))?
        .set_pkce_verifier(pkce_verifier)
        .request_async(http_client)
        .await
        .map_err(|e| OAuth2Error::TokenExchange(e.to_string()))?;

    // 11. Extract tokens
    let access_token = token_response.access_token().secret().clone();
    let refresh_token = token_response.refresh_token().map(|t| t.secret().clone());
    let expires_at = token_response.expires_in().map(|d| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before UNIX epoch")
            .as_secs()
            + d.as_secs()
    });

    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at,
        token_url: token_endpoint,
        client_id: client_id.to_string(),
    })
}

/// Perform the Device Code flow (RFC 8628).
///
/// 1. Discovers the OIDC provider metadata from the issuer URL.
/// 2. Requests a device authorization (device code + user code).
/// 3. Prints the verification URI and user code to stderr.
/// 4. Polls the token endpoint until the user authorizes (or times out).
async fn device_code_flow(
    http_client: &reqwest::Client,
    issuer_url: &str,
    client_id: &str,
) -> Result<OAuthTokens, OAuth2Error> {
    use std::time::{SystemTime, UNIX_EPOCH};

    use openidconnect::{
        core::CoreClient, ClientId, DeviceAuthorizationResponse, DeviceAuthorizationUrl,
        EmptyExtraDeviceAuthorizationFields, IssuerUrl, OAuth2TokenResponse, Scope,
    };

    // 1. Discover provider metadata (for token endpoint, etc.)
    let issuer = IssuerUrl::new(issuer_url.to_string())
        .map_err(|e| OAuth2Error::Discovery(format!("invalid issuer URL '{issuer_url}': {e}")))?;

    // Obtain the device_authorization_url from the OIDC metadata provider.
    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct DeviceEndpointProviderMetadata {
        device_authorization_endpoint: DeviceAuthorizationUrl,
    }
    impl openidconnect::AdditionalProviderMetadata for DeviceEndpointProviderMetadata {}
    type DeviceProviderMetadata = openidconnect::ProviderMetadata<
        DeviceEndpointProviderMetadata,
        openidconnect::core::CoreAuthDisplay,
        openidconnect::core::CoreClientAuthMethod,
        openidconnect::core::CoreClaimName,
        openidconnect::core::CoreClaimType,
        openidconnect::core::CoreGrantType,
        openidconnect::core::CoreJweContentEncryptionAlgorithm,
        openidconnect::core::CoreJweKeyManagementAlgorithm,
        openidconnect::core::CoreJsonWebKey,
        openidconnect::core::CoreResponseMode,
        openidconnect::core::CoreResponseType,
        openidconnect::core::CoreSubjectIdentifierType,
    >;

    let provider_metadata = DeviceProviderMetadata::discover_async(issuer, http_client)
        .await
        .map_err(|e| OAuth2Error::Discovery(e.to_string()))?;

    let token_endpoint = provider_metadata
        .token_endpoint()
        .ok_or(OAuth2Error::MissingTokenEndpoint)?
        .to_string();

    // Use the custom metadata to get the device_authorization_endpoint
    let device_authorization_endpoint = provider_metadata
        .additional_metadata()
        .device_authorization_endpoint
        .clone();

    // 2. Build the OIDC client with the device authorization URL set
    let oidc_client = CoreClient::from_provider_metadata(
        provider_metadata,
        ClientId::new(client_id.to_string()),
        None,
    )
    .set_device_authorization_url(device_authorization_endpoint)
    .set_auth_type(AuthType::RequestBody);

    // 3. Request device authorization
    let device_auth_response: DeviceAuthorizationResponse<EmptyExtraDeviceAuthorizationFields> =
        oidc_client
            .exchange_device_code()
            .add_scope(Scope::new("offline_access".to_string()))
            .request_async(http_client)
            .await
            .map_err(|e| OAuth2Error::DeviceAuthorization(e.to_string()))?;

    // 4. Print instructions to the user
    let verification_uri = device_auth_response.verification_uri().as_str();
    let user_code = device_auth_response.user_code().secret();

    eprintln!();
    eprintln!("To authenticate, open the following URL in your browser:");
    eprintln!("  {verification_uri}");
    eprintln!();
    eprintln!("And enter this code: {user_code}");
    eprintln!();
    eprintln!("Waiting for authorization...");

    // Try to open the verification URI in the browser (best effort)
    if let Some(complete_uri) = device_auth_response.verification_uri_complete() {
        let _ = open::that(complete_uri.secret().as_str());
    }

    // 5. Poll the token endpoint
    let token_response = oidc_client
        .exchange_device_access_token(&device_auth_response)
        .map_err(|e| OAuth2Error::DeviceAuthorization(e.to_string()))?
        .request_async(http_client, tokio::time::sleep, None)
        .await
        .map_err(|e| OAuth2Error::TokenExchange(e.to_string()))?;

    // 6. Extract tokens
    let access_token = token_response.access_token().secret().clone();
    let refresh_token = token_response.refresh_token().map(|t| t.secret().clone());
    let expires_at = token_response.expires_in().map(|d| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before UNIX epoch")
            .as_secs()
            + d.as_secs()
    });

    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at,
        token_url: token_endpoint,
        client_id: client_id.to_string(),
    })
}
