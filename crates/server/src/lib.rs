//! MessageNote 同步服务端。
//!
//! 单用户、公网部署、TLS 交给反向代理（Caddy）终结。
//!
//! 拆成 lib + bin 两个目标：lib 让端到端测试能在一个进程里把**真实的**
//! axum 服务端跑起来（绑 0 端口），不必去 spawn 二进制、也不必占固定端口。

pub mod api;
pub mod blobs;
pub mod error;
pub mod export;
pub mod store;

use std::sync::Arc;

use tokio::net::TcpListener;

pub use api::AppState;
pub use blobs::Blobs;
pub use error::{ServerError, ServerResult};
pub use store::Store;

/// 直接用给定的 listener 提供服务。
///
/// 与 bin 里的用法分开：bin 需要优雅退出，测试不需要。
pub async fn serve(listener: TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    axum::serve(listener, api::router(state)).await
}
