//! One refusal type for the whole plain-HTTP door (soma/ellmephant#233).
//!
//! A handler used to answer a refusal by constructing a ready-made `Response`:
//!
//! ```ignore
//! Err(e) => return Err(ApiError::store(json, e)),
//! ```
//!
//! That sentence was written ~80 times, and each copy re-decided three things the door should decide
//! once: the status, the prose, and whether the caller asked for JSON. Worse, it could not be
//! written with `?`, because a `Response` is already the finished answer.
//!
//! [`ApiError`] is the missing value: status + prose + the caller's format. A handler returns
//! `ApiResult<Response>` and answers a refusal with `?`. The text-vs-JSON decision — and the
//! `{"error","status"}` envelope — lives in exactly one place, [`ApiError::into_response`].
//!
//! The status for a store error comes from the VARIANT, not from a prefix of its message
//! (`error_status` used to sniff `"forbidden:"`). A variant cannot be misspelled by a new message.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// A handler's refusal, before it is a response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    /// The caller asked for `?format=json`. Carried because `IntoResponse` never sees the request.
    json: bool,
    /// Send `WWW-Authenticate: Bearer` — the 401 that tells a client what to present.
    challenge: bool,
}

/// A handler's result: a finished success is still a `Response` (its body chose its own shape),
/// while a failure is the value that carries the choice.
pub type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    /// A refusal with an explicit status.
    pub fn new(json: bool, status: StatusCode, message: impl ToString) -> Self {
        Self {
            status,
            message: message.to_string(),
            json,
            challenge: false,
        }
    }

    /// 400: the request is malformed, and the prose says what to send instead.
    pub fn bad(json: bool, message: impl ToString) -> Self {
        Self::new(json, StatusCode::BAD_REQUEST, message)
    }

    /// 404: the row (or the past state) does not exist.
    pub fn not_found(json: bool, message: impl ToString) -> Self {
        Self::new(json, StatusCode::NOT_FOUND, message)
    }

    /// 500: the door failed, not the caller.
    pub fn internal(json: bool, message: impl ToString) -> Self {
        Self::new(json, StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// 401 with the bare `WWW-Authenticate: Bearer` challenge, and nothing more.
    pub fn unauthorized(json: bool) -> Self {
        Self::new(json, StatusCode::UNAUTHORIZED, "unauthorized").challenging()
    }

    /// A store refusal: the status is the `Error` VARIANT's job, not a copy's, so a door cannot
    /// map `Forbidden` to a 400 by forgetting a prefix test.
    pub fn store(json: bool, error: impl std::borrow::Borrow<crate::error::Error>) -> Self {
        let error = error.borrow();
        Self::new(json, status_of(error), error)
    }

    /// Add the `WWW-Authenticate: Bearer` challenge.
    pub fn challenging(mut self) -> Self {
        self.challenge = true;
        self
    }

    /// The status this refusal will carry.
    pub fn status(&self) -> StatusCode {
        self.status
    }
}

/// The status a store error answers with: 403 refused, 404 absent, 413 too big, 503 no index,
/// 400 otherwise. The variant is the fact; the message is only prose.
fn status_of(error: &crate::error::Error) -> StatusCode {
    use crate::error::Error;
    match error {
        Error::Forbidden(_) => StatusCode::FORBIDDEN,
        Error::NotFound(_) | Error::NoProject(_) => StatusCode::NOT_FOUND,
        Error::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        Error::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        // The store's own machinery (a poisoned lock, a task that did not join) is not the caller's
        // mistake, so it earns a 500 rather than being flattened into the 400 for a bad argument.
        Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}

/// The same mapping for a message a caller-layer (not a `store` call) returned as prose. Kept for
/// the one place a verb's helper hands back a `String`, so the two paths cannot disagree.
pub fn status_of_message(message: &str) -> StatusCode {
    if message.starts_with("forbidden:") {
        StatusCode::FORBIDDEN
    } else if message.starts_with("no such memory:") || message.starts_with("no such project:") {
        StatusCode::NOT_FOUND
    } else if message.starts_with("body too long:") {
        StatusCode::PAYLOAD_TOO_LARGE
    } else if message.starts_with("index unavailable:") {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_REQUEST
    }
}

/// The ONLY text-vs-JSON decision on this door (soma/ellmephant#233).
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let Self {
            status,
            message,
            json,
            challenge,
        } = self;
        let body = if json {
            serde_json::json!({ "error": message, "status": status.as_u16() }).to_string()
        } else {
            message
        };
        let content_type = if json {
            "application/json"
        } else {
            "text/plain; charset=utf-8"
        };
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .expect("http error");
        if challenge {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn the_variant_decides_the_status() {
        assert_eq!(
            status_of(&Error::Forbidden("no".into())),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status_of(&Error::NotFound("gone".into())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&Error::NoProject("gone".into())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&Error::TooLarge("body too long: 9".into())),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            status_of(&Error::Unavailable("no vec".into())),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status_of(&Error::Invalid("nope".into())),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(&Error::Internal("lock poisoned".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // A message that LOOKS like a not-found is not one: the variant is the fact.
        assert_eq!(
            status_of(&Error::Invalid("no such memory: x".into())),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn json_is_one_envelope_and_text_is_one_line() {
        let json = ApiError::bad(true, "bad thing").into_response();
        assert_eq!(json.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json.headers()[header::CONTENT_TYPE], "application/json");

        let text = ApiError::not_found(false, "no such memory: k").into_response();
        assert_eq!(text.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            text.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn only_the_challenge_carries_www_authenticate() {
        let yes = ApiError::unauthorized(false).into_response();
        assert_eq!(yes.headers()[header::WWW_AUTHENTICATE], "Bearer");
        let no = ApiError::new(false, StatusCode::FORBIDDEN, "no").into_response();
        assert!(no.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    #[test]
    fn store_errors_borrow_or_own() {
        let owned = Error::Forbidden("no".into());
        assert_eq!(
            ApiError::store(false, &owned).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ApiError::store(false, owned).status(),
            StatusCode::FORBIDDEN
        );
    }
}
