use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    json: bool,
    challenge: bool,
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    pub fn new(json: bool, status: StatusCode, message: impl ToString) -> Self {
        Self {
            status,
            message: message.to_string(),
            json,
            challenge: false,
        }
    }

    pub fn bad(json: bool, message: impl ToString) -> Self {
        Self::new(json, StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(json: bool, message: impl ToString) -> Self {
        Self::new(json, StatusCode::NOT_FOUND, message)
    }

    pub fn internal(json: bool, message: impl ToString) -> Self {
        Self::new(json, StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    pub fn unauthorized(json: bool) -> Self {
        Self::new(json, StatusCode::UNAUTHORIZED, "unauthorized").challenging()
    }

    pub fn store(json: bool, error: impl std::borrow::Borrow<crate::error::Error>) -> Self {
        let error = error.borrow();
        Self::new(json, status_of(error), error)
    }

    pub fn challenging(mut self) -> Self {
        self.challenge = true;
        self
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
}

fn status_of(error: &crate::error::Error) -> StatusCode {
    use crate::error::Error;
    match error {
        Error::Forbidden(_) => StatusCode::FORBIDDEN,
        Error::NotFound(_) | Error::NoProject(_) => StatusCode::NOT_FOUND,
        Error::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        Error::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}

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
