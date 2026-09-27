use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("已停止")]
    Stopped,
}

impl Error {
    pub fn msg(s: impl Into<String>) -> Self {
        Self::Message(s.into())
    }
}
