//! Map [`ServiceError`] to an HTTP response.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::service::ServiceError;

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: String,
}

pub fn map(err: ServiceError) -> Response {
    let (status, code) = match &err {
        ServiceError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        ServiceError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
        ServiceError::InvalidInput(_) => (StatusCode::BAD_REQUEST, "invalid_input"),
        ServiceError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
        ServiceError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        ServiceError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_error"),
        ServiceError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, "database_error"),
        ServiceError::Unifi(oxy_unifi::UnifiError::Forbidden(_)) => {
            (StatusCode::FORBIDDEN, "unifi_forbidden")
        }
        ServiceError::Unifi(oxy_unifi::UnifiError::NotFound(_)) => {
            (StatusCode::NOT_FOUND, "unifi_not_found")
        }
        ServiceError::Unifi(oxy_unifi::UnifiError::RateLimited { .. }) => {
            (StatusCode::TOO_MANY_REQUESTS, "unifi_rate_limited")
        }
        ServiceError::Unifi(_) => (StatusCode::BAD_GATEWAY, "unifi_error"),
        ServiceError::Airhouse(crate::airhouse::AirhouseError::Disabled) => {
            (StatusCode::SERVICE_UNAVAILABLE, "airhouse_disabled")
        }
        ServiceError::Airhouse(_) => (StatusCode::BAD_GATEWAY, "airhouse_error"),
        ServiceError::NotImplemented(_) => (StatusCode::NOT_IMPLEMENTED, "not_implemented"),
        ServiceError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };
    let message = err.to_string();
    // The one place every cameras route error becomes a response, so the one
    // place its cause can be logged. Without this the request span's
    // `request failed` line carried a status and nothing else: 55 bare `502`s
    // an hour on `POST /api/control/compliance-reports` in prod on 2026-09-10,
    // with the upstream's actual complaint only in a body nobody logs. Emitted
    // inside the request span, so it carries the route and the trace id.
    if status.is_server_error() {
        tracing::error!(status = status.as_u16(), code, error = %message, "cameras request failed");
    } else {
        tracing::debug!(status = status.as_u16(), code, error = %message, "cameras request rejected");
    }
    let body = ApiErrorBody { code, message };
    (status, Json(body)).into_response()
}
