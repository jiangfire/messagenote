use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("数据库错误：{0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("文件系统错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Core(#[from] messagenote_core::error::CoreError),

    #[error("{0}")]
    Msg(String),
}

/// 面向客户端的错误响应。
///
/// **细节只进日志，不进响应体。** 这是一个暴露在公网上、装着用户全部笔记的
/// 服务，把 SQL 错误原文回给请求方等于白送一份内部结构情报。
impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self, "请求处理失败");
        (StatusCode::INTERNAL_SERVER_ERROR, "服务端内部错误").into_response()
    }
}

impl Serialize for ServerError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type ServerResult<T> = Result<T, ServerError>;
