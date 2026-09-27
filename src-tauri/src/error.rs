use serde::Serialize;

/// 桌面端的统一错误类型。
///
/// 在 P0 里这个类型住在 `src/error.rs`；抽出内核后它留在客户端，
/// 因为 `Sqlite` 和 `Io` 变体只对"本地优先的桌面端"有意义。
/// 服务端有它自己的对应物，两者都把内核错误作为一个来源包进来。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("数据库错误：{0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("文件系统错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Core(#[from] messagenote_core::error::CoreError),

    #[error("{0}")]
    Msg(String),
}

/// Tauri 的 `#[tauri::command]` 要求错误类型实现 `Serialize`，
/// 这样前端 `invoke()` 的 reject 拿到的是可读字符串而不是
/// 一个没有信息的 "unknown error"。
impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    /// 把一句话包成客户端错误。
    ///
    /// 主要是给共享层用的：`messagenote-store` 的规范化和解析函数返回
    /// `&'static str` 或实现了 `Display` 的小错误，各自只需要一句话说明
    /// "哪里不合法"，由两端包成自己的错误类型。
    pub fn msg(text: impl Into<String>) -> Self {
        AppError::Msg(text.into())
    }
}
