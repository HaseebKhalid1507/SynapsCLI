use thiserror::Error;

#[derive(Error, Debug)]
pub enum RuntimeError {
    #[error("API error: {0}")]
    Api(#[from] reqwest::Error),
    #[error("Auth error: {0}")]
    Auth(String),
    #[error("Config error: {0}")]
    Config(String),
    #[error("Session error: {0}")]
    Session(String),
    #[error("Tool execution failed: {0}")]
    Tool(String),
    #[error("Request timed out")]
    Timeout,
    #[error("Operation canceled")]
    Canceled,
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_error_display() {
        assert_eq!(
            format!("{}", RuntimeError::Auth("bad token".into())),
            "Auth error: bad token"
        );

        assert_eq!(
            format!("{}", RuntimeError::Config("missing".into())),
            "Config error: missing"
        );

        assert_eq!(
            format!("{}", RuntimeError::Tool("failed".into())),
            "Tool execution failed: failed"
        );

        assert_eq!(
            format!("{}", RuntimeError::Session("not found".into())),
            "Session error: not found"
        );

        assert_eq!(
            format!("{}", RuntimeError::Timeout),
            "Request timed out"
        );

        assert_eq!(
            format!("{}", RuntimeError::Canceled),
            "Operation canceled"
        );
    }

    #[test]
    fn test_runtime_error_to_string() {
        assert_eq!(
            RuntimeError::Auth("bad token".into()).to_string(),
            "Auth error: bad token"
        );

        assert_eq!(
            RuntimeError::Config("missing".into()).to_string(),
            "Config error: missing"
        );

        assert_eq!(
            RuntimeError::Tool("failed".into()).to_string(),
            "Tool execution failed: failed"
        );

        assert_eq!(
            RuntimeError::Session("not found".into()).to_string(),
            "Session error: not found"
        );

        assert_eq!(
            RuntimeError::Timeout.to_string(),
            "Request timed out"
        );

        assert_eq!(
            RuntimeError::Canceled.to_string(),
            "Operation canceled"
        );
    }
}
