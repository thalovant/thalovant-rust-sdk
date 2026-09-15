//! The authorization-code grant with PKCE (RFC 7636), for a client that can
//! open a browser.
//!
//! [`ControlPlane::login_with_browser`](crate::control::ControlPlane::login_with_browser)
//! is the device grant, and it exists for something that *cannot* open one:
//! somebody reads a code off one screen and types it into another. A desktop
//! tool can open the browser itself and be handed the answer back, and asking
//! its user to copy a code between two windows on the same machine is a worse
//! experience than the one every other tool on that machine offers.
//!
//! ```no_run
//! # use thalovant::native_auth::{begin_native_sign_in, NativeSignInOptions};
//! let begun = begin_native_sign_in(NativeSignInOptions {
//!     client_id: "my-app".into(),
//!     redirect_uri: "http://127.0.0.1:8765/".into(),
//!     ..Default::default()
//! })?;
//! // open begun.authorization_url, then when the browser comes back:
//! let code = begun.code_from(&redirect);   // None when it is not ours
//! # Ok::<(), thalovant::errors::ThalovantError>(())
//! ```
//!
//! `begun.verifier` never leaves the process and never enters the browser.
//! That is what PKCE is for: a code intercepted by whatever else claimed the
//! redirect is useless without it.

use base64::{engine::general_purpose, Engine as _};
use rand::RngCore;
use sha2::{Digest, Sha256};
use url::Url;

use crate::errors::{Result, ThalovantError};

/// Where a person approves the request.
pub const DEFAULT_DASHBOARD_URL: &str = "https://dash.thalovant.com";

/// The three a phone needs; also the three a free plan may mint.
pub const DEFAULT_NATIVE_SCOPES: [&str; 3] = ["hubs:read", "clients:read", "clients:write"];

/// Inputs for [`begin_native_sign_in`].
///
/// `redirect_uri` must be one the API has registered for `client_id`; the
/// authorization endpoint matches it exactly and refuses anything else, so it
/// cannot be turned into an open redirect.
#[derive(Debug, Clone, Default)]
pub struct NativeSignInOptions {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Option<Vec<String>>,
    pub dashboard_url: Option<String>,
}

/// One sign-in attempt in progress. Keep it until the browser comes back; it
/// holds the two secrets that make the round trip safe.
#[derive(Debug, Clone)]
pub struct NativeSignIn {
    /// Open this in a browser.
    pub authorization_url: String,
    /// Proves the redirect answers *this* attempt and not a replayed one.
    pub state: String,
    /// Never send this to the browser. Exchanged with the code, once.
    pub verifier: String,
}

fn base64_url(raw: &[u8]) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

fn random_url_safe(size: usize) -> String {
    let mut raw = vec![0u8; size];
    rand::thread_rng().fill_bytes(&mut raw);
    base64_url(&raw)
}

/// A PKCE verifier: 64 random bytes, base64url, no padding.
pub fn new_verifier() -> String {
    random_url_safe(64)
}

/// The S256 challenge for a verifier.
pub fn challenge_for(verifier: &str) -> String {
    base64_url(&Sha256::digest(verifier.as_bytes()))
}

/// Whether a URL belongs to Thalovant, for a caller that wants to show where
/// it is about to send somebody. Scheme and host only: a display check, not an
/// authorization one.
pub fn is_thalovant_url(raw: &str) -> bool {
    let Ok(parsed) = Url::parse(raw) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    match parsed.host_str() {
        Some(host) => {
            let host = host.to_ascii_lowercase();
            host == "thalovant.com" || host.ends_with(".thalovant.com")
        }
        None => false,
    }
}

/// Start a sign-in, returning the URL to open and the secrets to keep.
pub fn begin_native_sign_in(options: NativeSignInOptions) -> Result<NativeSignIn> {
    let client_id = options.client_id.trim().to_string();
    let redirect_uri = options.redirect_uri.trim().to_string();
    if client_id.is_empty() {
        return Err(ThalovantError::Api(
            "client_id is required to start a sign-in".to_string(),
        ));
    }
    if redirect_uri.is_empty() {
        return Err(ThalovantError::Api(
            "redirect_uri is required to start a sign-in".to_string(),
        ));
    }
    let verifier = new_verifier();
    let state = random_url_safe(24);
    let scopes = options.scopes.unwrap_or_else(|| {
        DEFAULT_NATIVE_SCOPES
            .iter()
            .map(|scope| (*scope).to_string())
            .collect()
    });
    let dashboard = options
        .dashboard_url
        .unwrap_or_else(|| DEFAULT_DASHBOARD_URL.to_string());
    let dashboard = dashboard.trim_end_matches('/');
    let mut url = Url::parse(&format!("{dashboard}/authorize"))
        .map_err(|_| ThalovantError::Api("could not build the sign-in address".to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("code_challenge", &challenge_for(&verifier))
        // S256 only. `plain` is refused by the API, and offering it here would
        // only give a caller a way to ask for the weaker one.
        .append_pair("code_challenge_method", "S256")
        .append_pair("scope", &scopes.join(" "))
        .append_pair("state", &state);
    Ok(NativeSignIn {
        authorization_url: url.to_string(),
        state,
        verifier,
    })
}

impl NativeSignIn {
    /// The authorization code out of the redirect the browser came back with,
    /// or `None` when it is not an answer to this attempt.
    ///
    /// `None` rather than an error on a state mismatch, a missing code, or an
    /// `error=` response: all three mean "do not continue", and a caller that
    /// handles them alike cannot accidentally treat one of them as success.
    pub fn code_from(&self, redirect: &str) -> Option<String> {
        let parsed = Url::parse(redirect).ok()?;
        let mut state = None;
        let mut code = None;
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "state" => state = Some(value.into_owned()),
                "code" => code = Some(value.into_owned()),
                _ => {}
            }
        }
        if state.as_deref() != Some(self.state.as_str()) {
            return None;
        }
        code.filter(|value| !value.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The authorization-code grant, which every client that needed it wrote
    // for itself until this existed. What is tested is what is a security bug
    // when wrong and looks fine when wrong.

    fn begin(client_id: &str, redirect_uri: &str) -> NativeSignIn {
        begin_native_sign_in(NativeSignInOptions {
            client_id: client_id.to_string(),
            redirect_uri: redirect_uri.to_string(),
            ..Default::default()
        })
        .expect("a sign-in starts")
    }

    fn query(url: &str, name: &str) -> Option<String> {
        Url::parse(url)
            .ok()?
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    #[test]
    fn the_challenge_is_the_s256_of_the_verifier() {
        let begun = begin("thalovant-cli", "http://127.0.0.1:8765/");
        let expected = base64_url(&Sha256::digest(begun.verifier.as_bytes()));
        assert_eq!(
            query(&begun.authorization_url, "code_challenge"),
            Some(expected.clone())
        );
        assert_eq!(challenge_for(&begun.verifier), expected);
    }

    #[test]
    fn the_verifier_never_reaches_the_browser() {
        let begun = begin("app", "app://auth");
        assert!(!begun.authorization_url.contains(&begun.verifier));
    }

    #[test]
    fn only_s256_is_offered() {
        let begun = begin("app", "app://auth");
        assert_eq!(
            query(&begun.authorization_url, "code_challenge_method"),
            Some("S256".to_string())
        );
    }

    #[test]
    fn every_attempt_gets_its_own_verifier_and_state() {
        let first = begin("app", "app://auth");
        let second = begin("app", "app://auth");
        assert_ne!(first.verifier, second.verifier);
        assert_ne!(first.state, second.state);
        assert_ne!(new_verifier(), new_verifier());
    }

    #[test]
    fn the_request_carries_what_the_authorize_endpoint_matches_on() {
        let begun = begin_native_sign_in(NativeSignInOptions {
            client_id: "thalovant-cli".to_string(),
            redirect_uri: "http://127.0.0.1:8765/".to_string(),
            scopes: Some(vec!["hubs:read".to_string(), "clients:write".to_string()]),
            ..Default::default()
        })
        .expect("a sign-in starts");
        let url = &begun.authorization_url;
        assert_eq!(query(url, "client_id"), Some("thalovant-cli".to_string()));
        assert_eq!(
            query(url, "redirect_uri"),
            Some("http://127.0.0.1:8765/".to_string())
        );
        assert_eq!(query(url, "response_type"), Some("code".to_string()));
        assert_eq!(
            query(url, "scope"),
            Some("hubs:read clients:write".to_string())
        );
        assert_eq!(query(url, "state"), Some(begun.state.clone()));
        assert!(url.starts_with("https://dash.thalovant.com/authorize?"));
    }

    #[test]
    fn the_default_scopes_are_the_three_a_free_plan_may_mint() {
        let begun = begin("app", "app://auth");
        assert_eq!(
            query(&begun.authorization_url, "scope"),
            Some(DEFAULT_NATIVE_SCOPES.join(" "))
        );
    }

    #[test]
    fn a_redirect_answering_a_different_attempt_is_refused() {
        let begun = begin("app", "app://auth");
        assert_eq!(
            begun.code_from("app://auth?code=abc&state=somebody-elses"),
            None
        );
        assert_eq!(
            begun.code_from(&format!("app://auth?code=abc&state={}", begun.state)),
            Some("abc".to_string())
        );
    }

    #[test]
    fn no_code_or_an_error_instead_is_not_success() {
        let begun = begin("app", "app://auth");
        for redirect in [
            format!("app://auth?state={}", begun.state),
            format!("app://auth?code=&state={}", begun.state),
            format!("app://auth?error=access_denied&state={}", begun.state),
            "app://auth".to_string(),
        ] {
            assert_eq!(
                begun.code_from(&redirect),
                None,
                "{redirect} was treated as success"
            );
        }
    }

    #[test]
    fn a_code_with_escaped_characters_survives_the_round_trip() {
        let begun = begin("app", "app://auth");
        assert_eq!(
            begun.code_from(&format!(
                "app://auth?code=a%2Bb%2Fc%3D&state={}",
                begun.state
            )),
            Some("a+b/c=".to_string())
        );
    }

    #[test]
    fn an_empty_client_or_redirect_is_refused_here_rather_than_at_the_api() {
        assert!(begin_native_sign_in(NativeSignInOptions {
            client_id: String::new(),
            redirect_uri: "app://auth".to_string(),
            ..Default::default()
        })
        .is_err());
        assert!(begin_native_sign_in(NativeSignInOptions {
            client_id: "app".to_string(),
            redirect_uri: "   ".to_string(),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn a_thalovant_url_is_recognised_by_scheme_and_host() {
        assert!(is_thalovant_url("https://dash.thalovant.com/authorize?x=1"));
        assert!(is_thalovant_url("https://thalovant.com"));
        assert!(!is_thalovant_url("http://dash.thalovant.com"));
        // The one that matters: a lookalike host ending in the same letters.
        assert!(!is_thalovant_url("https://dash.thalovant.com.evil.test"));
        assert!(!is_thalovant_url("https://notthalovant.com"));
        assert!(!is_thalovant_url("nonsense"));
    }
}
