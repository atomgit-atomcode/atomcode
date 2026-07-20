use std::sync::Arc;

use atomcode_auth::gateway_crypto::{self, SignInput};
use atomcode_auth::oauth::{get_stored_auth, get_valid_auth_session};

use super::{RequestSigner, RequestSigningError, SignedAuth};

struct AtomGitRequestSigner {
    path: String,
}

impl RequestSigner for AtomGitRequestSigner {
    fn sign(&self, body: &[u8]) -> Result<SignedAuth, RequestSigningError> {
        let auth = get_valid_auth_session()
            .map_err(|error| RequestSigningError::CredentialsUnavailable(error.to_string()))?;
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|error| {
            RequestSigningError::SigningFailed(format!("secure nonce generation failed: {error}"))
        })?;
        let timestamp_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let headers = gateway_crypto::signer()
            .sign(SignInput {
                method: "POST",
                path: &self.path,
                body,
                oauth_token: &auth.access_token,
                user_id: &auth.user_id,
                timestamp_unix,
                nonce,
            })
            .map_err(|error| RequestSigningError::SigningFailed(error.to_string()))?
            .headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect();
        Ok(SignedAuth {
            bearer: Some(auth.access_token),
            headers,
        })
    }
}

pub fn atomgit_request_signer(base_url: &str) -> Result<Arc<dyn RequestSigner>, String> {
    let auth = get_stored_auth()
        .ok_or_else(|| "AtomGit gateway requires login — run `/login` first".to_string())?;
    if auth.user.id.is_empty() || auth.access_token.is_empty() {
        return Err("AtomGit gateway requires login — run `/login` first".to_string());
    }
    Ok(Arc::new(AtomGitRequestSigner {
        path: gateway_crypto::canonical_chat_completions_path(base_url),
    }))
}

pub use gateway_crypto::{is_atomgit_gateway, signer_available};

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_auth::oauth::{logout, save_auth, AuthInfo, UserInfo};

    fn auth(user_id: &str, token: &str) -> AuthInfo {
        AuthInfo {
            access_token: token.into(),
            refresh_token: None,
            token_type: "bearer".into(),
            expires_in: None,
            created_at: 1,
            user: UserInfo {
                id: user_id.into(),
                username: "tester".into(),
                name: None,
                email: None,
                avatar_url: None,
            },
        }
    }

    #[test]
    #[serial_test::serial]
    fn signer_does_not_reuse_cached_credentials_after_logout() {
        logout().unwrap();
        save_auth(&auth("user-1", "token-1")).unwrap();
        let signer = atomgit_request_signer("https://api.atomgit.com/v1").unwrap();

        logout().unwrap();

        let error = signer.sign(b"{}").unwrap_err();
        assert!(matches!(
            error,
            super::super::RequestSigningError::CredentialsUnavailable(_)
        ));
    }

    #[test]
    #[serial_test::serial]
    fn existing_signer_uses_replacement_auth_snapshot() {
        logout().unwrap();
        save_auth(&auth("user-1", "token-1")).unwrap();
        let signer = atomgit_request_signer("https://api.atomgit.com/v1").unwrap();

        save_auth(&auth("user-2", "token-2")).unwrap();

        match signer.sign(b"{}") {
            Ok(signed) => assert_eq!(signed.bearer.as_deref(), Some("token-2")),
            Err(RequestSigningError::SigningFailed(_)) if !signer_available() => {}
            Err(error) => panic!("replacement credentials were not accepted: {error}"),
        }
        logout().unwrap();
    }
}
