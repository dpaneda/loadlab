use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoadlabError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("invalid expression: {0}")]
    InvalidExpression(String),

    #[error("procfs error: {0}")]
    Procfs(String),

    #[error("memory mapping error: {0}")]
    MemoryMap(String),

    #[error("i/o error: {0}")]
    Io(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// No credentials, or credentials that do not match the configured token.
    ///
    /// The message deliberately says nothing about which of the two it was. Distinguishing them
    /// would tell an unauthenticated caller whether a guessed token was close, which is the one
    /// piece of feedback an attacker gets for free otherwise.
    #[error("unauthorized: a valid bearer token is required")]
    Unauthorized,

    /// The request body was larger than the configured limit.
    #[error("request body too large: the limit is {limit} bytes")]
    PayloadTooLarge { limit: usize },

    /// More mutating requests than the limiter allows.
    ///
    /// Carries how long to wait, which becomes `Retry-After`. A rate limit that does not say when
    /// to come back is a shrug, not a limit.
    #[error("too many requests: retry in {retry_after_seconds}s")]
    TooManyRequests { retry_after_seconds: u64 },
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for LoadlabError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::InvalidConfig(_) | Self::InvalidExpression(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Procfs(_)
            | Self::MemoryMap(_)
            | Self::Io(_)
            | Self::Network(_)
            | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let mut response = (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response();

        // `WWW-Authenticate` is what makes a 401 actionable: it names the scheme to use, so a client
        // does not have to guess whether it should be sending a token at all.
        if let Self::Unauthorized = self {
            response.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer realm=\"loadlab\""),
            );
        }
        // `Retry-After` in seconds, for the same reason.
        if let Self::TooManyRequests {
            retry_after_seconds,
        } = self
        {
            if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after_seconds.to_string()) {
                response
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
            }
        }
        response
    }
}
