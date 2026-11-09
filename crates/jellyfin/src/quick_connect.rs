//! Quick Connect: signing in by approving a code in another Jellyfin app.

use serde_json::{Value, json};

use crate::client::{Client, HttpError};
use crate::identity::{Secret, pct};

/// A login in progress: the code to show, and the secret to poll it with.
pub struct QuickConnectCode {
    /// What the viewer approves in another app.
    pub code: String,
    /// What polls for the approval and then signs in.
    pub secret: String,
}

impl Client {
    /// Whether the server allows Quick Connect.
    ///
    /// # Errors
    /// The request failed.
    pub fn quick_connect_enabled(&self) -> Result<bool, HttpError> {
        Ok(self.get("/QuickConnect/Enabled")? == Value::Bool(true))
    }

    /// Starts a login.
    ///
    /// # Errors
    /// The request failed, or the reply has no code.
    pub fn quick_connect_initiate(&self) -> Result<QuickConnectCode, HttpError> {
        let reply = self.post("/QuickConnect/Initiate", &Value::Null)?;
        let field = |k: &str| reply.get(k).and_then(Value::as_str).map(str::to_owned);
        match (field("Code"), field("Secret")) {
            (Some(code), Some(secret)) => Ok(QuickConnectCode { code, secret }),
            _ => Err(HttpError::transport("Quick Connect reply has no code")),
        }
    }

    /// Whether the viewer has approved the code.
    ///
    /// # Errors
    /// The request failed; `HttpError::Status(404)` means the code expired.
    pub fn quick_connect_approved(&self, secret: &str) -> Result<bool, HttpError> {
        let status = self.get(&format!("/QuickConnect/Connect?secret={}", pct(secret)))?;
        Ok(status.get("Authenticated") == Some(&Value::Bool(true)))
    }

    /// Signs in with an approved code: the user id and access token.
    ///
    /// # Errors
    /// The request failed, or the reply has no token.
    pub fn quick_connect_login(&self, secret: &str) -> Result<(String, Secret), HttpError> {
        let login = self.post(
            "/Users/AuthenticateWithQuickConnect",
            &json!({ "Secret": secret }),
        )?;
        let user_id = login.pointer("/User/Id").and_then(Value::as_str);
        let token = login.get("AccessToken").and_then(Value::as_str);
        let (Some(user_id), Some(token)) = (user_id, token) else {
            return Err(HttpError::transport(
                "Quick Connect login reply has no token",
            ));
        };
        Ok((user_id.to_owned(), Secret::new(token.to_owned())))
    }
}
