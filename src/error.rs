//! Expected failures and their mapping to `google.rpc.Status` (gRPC code + `ErrorInfo`, plus
//! `BadRequest` for invalid input): the error model of the Java `GrpcErrorHandler`.

use tonic::{Code, Status};
use tonic_types::{BadRequest, ErrorDetail, ErrorInfo, StatusExt};

pub const DOMAIN: &str = "message-service.miqui.dev";
pub const INTERNAL_REASON: &str = "INTERNAL_SERVER_ERROR";
pub const BAD_INPUT_MESSAGE: &str =
    "The request content was invalid or failed validation constraints.";
pub const INTERNAL_MESSAGE: &str = "An unexpected error occurred.";

pub const UNIQUE_VIOLATION: &str = "23505";
pub const FK_VIOLATION: &str = "23503";

/// One invalid field: `google.rpc.BadRequest.FieldViolation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldViolation {
    pub field: String,
    pub description: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// INVALID_ARGUMENT with every failing field at once.
    #[error("{BAD_INPUT_MESSAGE}")]
    BadInput(Vec<FieldViolation>),
    #[error("{0}")]
    NotFound(String),
    /// Reason `CONFLICT` with the gRPC code that fits: ABORTED (stale version), ALREADY_EXISTS
    /// (duplicate email), FAILED_PRECONDITION (author still has messages).
    #[error("{message}")]
    Conflict { code: Code, message: String },
    /// Anything unexpected: logged, answered with a generic INTERNAL (no details leak).
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    pub fn code(&self) -> Code {
        match self {
            ApiError::BadInput(_) => Code::InvalidArgument,
            ApiError::NotFound(_) => Code::NotFound,
            ApiError::Conflict { code, .. } => *code,
            ApiError::Internal(_) => Code::Internal,
        }
    }

    /// `ErrorInfo.reason`: the stable code clients switch on.
    pub fn reason(&self) -> &'static str {
        match self {
            ApiError::BadInput(_) => "BAD_USER_INPUT",
            ApiError::NotFound(_) => "NOT_FOUND",
            ApiError::Conflict { .. } => "CONFLICT",
            ApiError::Internal(_) => INTERNAL_REASON,
        }
    }

    /// Client-facing message; never carries internal detail.
    pub fn client_message(&self) -> String {
        match self {
            ApiError::Internal(_) => INTERNAL_MESSAGE.to_owned(),
            other => other.to_string(),
        }
    }

    pub fn conflict(code: Code, message: impl Into<String>) -> Self {
        ApiError::Conflict {
            code,
            message: message.into(),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl From<ApiError> for Status {
    fn from(e: ApiError) -> Status {
        let info = ErrorInfo::new(e.reason(), DOMAIN, std::collections::HashMap::new());
        let mut details: Vec<ErrorDetail> = vec![info.into()];
        if let ApiError::BadInput(violations) = &e {
            let mut br = BadRequest::new(vec![]);
            for v in violations {
                br.add_violation(v.field.clone(), v.description.clone());
            }
            details.push(br.into());
        }
        Status::with_error_details_vec(e.code(), e.client_message(), details)
    }
}

/// `ErrorInfo.reason` of a status (metrics label, tests, clients); falls back to the bare gRPC
/// code name for statuses this service did not produce.
pub fn error_code_of(status: &Status) -> String {
    status
        .get_details_error_info()
        .map(|i| i.reason)
        .unwrap_or_else(|| format!("{:?}", status.code()).to_ascii_uppercase())
}

/// PostgreSQL SQLSTATE of a database error, if it is one.
pub fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code().map(|c| c.into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic_types::StatusExt;

    #[test]
    fn bad_input_carries_error_info_and_bad_request() {
        let st: Status = ApiError::BadInput(vec![FieldViolation {
            field: "title".into(),
            description: "title is required and cannot be blank".into(),
        }])
        .into();
        assert_eq!(st.code(), Code::InvalidArgument);
        assert_eq!(st.message(), BAD_INPUT_MESSAGE);
        let info = st.get_details_error_info().unwrap();
        assert_eq!(
            (info.reason.as_str(), info.domain.as_str()),
            ("BAD_USER_INPUT", DOMAIN)
        );
        let br = st.get_details_bad_request().unwrap();
        assert_eq!(br.field_violations.len(), 1);
        assert_eq!(br.field_violations[0].field, "title");
    }

    #[test]
    fn conflicts_keep_their_code_with_conflict_reason() {
        for code in [Code::Aborted, Code::AlreadyExists, Code::FailedPrecondition] {
            let st: Status = ApiError::conflict(code, "x").into();
            assert_eq!(st.code(), code);
            assert_eq!(error_code_of(&st), "CONFLICT");
            assert!(st.get_details_bad_request().is_none());
        }
    }

    #[test]
    fn not_found_reason() {
        let st: Status = ApiError::NotFound("gone".into()).into();
        assert_eq!(
            (st.code(), error_code_of(&st).as_str()),
            (Code::NotFound, "NOT_FOUND")
        );
    }

    #[test]
    fn internal_never_leaks_detail() {
        let st: Status = ApiError::Internal("password=hunter2 connection refused".into()).into();
        assert_eq!(st.code(), Code::Internal);
        assert_eq!(st.message(), INTERNAL_MESSAGE);
        assert_eq!(error_code_of(&st), INTERNAL_REASON);
        assert!(!format!("{st:?}").contains("hunter2"));
    }

    #[test]
    fn foreign_status_falls_back_to_code_name() {
        assert_eq!(
            error_code_of(&Status::resource_exhausted("big")),
            "RESOURCEEXHAUSTED"
        );
    }
}
