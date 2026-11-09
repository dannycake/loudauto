//! Where the web client's credentials come from: the login5 bearer token and the
//! client token, both minted by the librespot session.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{Result, session::SessionCell};

#[async_trait]
pub trait Tokens: Send + Sync {
    /// The login5 access token, sent as `authorization: Bearer <token>`.
    async fn bearer(&self) -> Result<String>;
    /// The `client-token` header value.
    async fn client_token(&self) -> Result<String>;
}

/// Tokens from whatever session the cell holds now, so a reconnect is picked up at
/// the next request. librespot caches both tokens until they expire, so asking on
/// every request costs nothing.
pub struct SessionTokens(pub Arc<SessionCell>);

#[async_trait]
impl Tokens for SessionTokens {
    async fn bearer(&self) -> Result<String> {
        Ok(self.0.get()?.login5().auth_token().await?.access_token)
    }

    async fn client_token(&self) -> Result<String> {
        Ok(self.0.get()?.spclient().client_token().await?)
    }
}
