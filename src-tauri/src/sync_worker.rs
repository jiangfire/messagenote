//! 后台自动同步。
//!
//! 为什么必须是后台线程，而不是"点一下同步一次"：多设备同步要真正有用，
//! **必须是自动的**。需要用户记得手动触发同步的方案等于没有同步 ——
//! 你永远不会在手机上想起来点它。
//!
//! ## 锁的纪律
//!
//! 这个模块里**只有 `run_once` 开头读配置那一下短暂持锁**，之后走网络的
//! 全程都不碰数据库锁。`sync_once` 只接受 `sync::LocalStore`（即 `Db`），
//! 不接受 `&Connection` —— 所以"网络期间持锁"在这里写不出来。
//!
//! 别把 `db.conn()` 的 guard 一路带到 `sync_once` 里去。`std::sync::Mutex`
//! 不可重入，那不是变慢，是启动后第一轮同步就死锁。

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::db::{self, Db};
use crate::http::HttpServerApi;
use crate::sync;

/// 自动同步间隔。单用户场景下数据量极小，这个频率的代价可以忽略。
const INTERVAL: Duration = Duration::from_secs(45);

/// 同步状态事件名，前端监听它来显示状态。
pub const STATUS_EVENT: &str = "sync://status";

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {    pub ok: bool,
    pub message: String,
    pub pushed: usize,
    pub pulled: usize,
    pub conflicts: usize,
}

impl SyncStatus {
    fn err(message: String) -> Self {
        Self {
            ok: false,
            message,
            pushed: 0,
            pulled: 0,
            conflicts: 0,
        }
    }
}

pub struct SyncWorker {
    wake: Mutex<Option<Sender<()>>>,
    last: Mutex<Option<SyncStatus>>,
}

impl SyncWorker {
    /// 请求立刻同步一次（前端"立即同步"按钮）。
    ///
    /// 命令本身立刻返回，真正的同步发生在后台线程上 ——
    /// 否则网络慢的时候整个界面会卡住。
    pub fn trigger(&self) {
        if let Ok(guard) = self.wake.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(());
            }
        }
    }

    /// 最近一次同步的结果。
    ///
    /// 前端挂载时必须**主动读一次**，不能只靠监听事件：后台线程在应用启动
    /// 时就会同步一次，而那一刻前端往往还没来得及注册监听器 ——
    /// 只靠事件的话，界面会一直停在"待同步"，直到 45 秒后的下一轮。
    /// 这类"推送 + 可查询"的组合，是所有事件驱动界面都会踩的坑。
    pub fn last(&self) -> Option<SyncStatus> {
        self.last.lock().ok().and_then(|g| g.clone())
    }
}

pub fn spawn(app: AppHandle) {
    let (tx, rx) = mpsc::channel::<()>();
    app.manage(SyncWorker {
        wake: Mutex::new(Some(tx)),
        last: Mutex::new(None),
    });

    std::thread::spawn(move || loop {
        // 先同步、再等待。反过来写的话，用户打开应用后要干等一个周期
        // 才会看到别的设备上的内容 —— 而"打开就是最新的"是本地优先应用
        // 最基本的预期。
        let status = run_once(&app);

        // 存下来供前端查询，再推一次供已在监听的界面即时更新
        if let Some(worker) = app.try_state::<SyncWorker>() {
            if let Ok(mut guard) = worker.last.lock() {
                *guard = Some(status.clone());
            }
        }
        // 失败不该弹窗打断用户 —— 笔记应用里同步失败通常只是"现在没网"
        let _ = app.emit(STATUS_EVENT, status);

        match rx.recv_timeout(INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    });
}

fn run_once(app: &AppHandle) -> SyncStatus {
    sync_with(&app.state::<Db>())
}

/// 同步一次：从读配置到落地结果。
///
/// 从 `run_once` 里抽出来，是为了让这条**生产路径**能被测试直接驱动 ——
/// `run_once` 要一个 `AppHandle`，而这里只要一个 `Db`。
///
/// **只有开头读配置那一下持有数据库锁。** 建 HTTP 客户端、走网络、落地结果
/// 全程不碰锁。往 `sync_once` 传 `&Connection` 之类的东西会让应用第一轮
/// 同步就死锁，`the_database_stays_available_...` 会拦住它。
fn sync_with(db: &Db) -> SyncStatus {
    let cfg = {
        let conn = match db.conn() {
            Ok(c) => c,
            Err(e) => return SyncStatus::err(e.to_string()),
        };
        match db::get_sync_config(&conn) {
            Ok(c) => c,
            Err(e) => return SyncStatus::err(e.to_string()),
        }
    };

    if cfg.url.is_empty() || cfg.token.is_empty() {
        return SyncStatus::err("尚未配置同步服务端".into());
    }

    let api = match HttpServerApi::new(&cfg.url, &cfg.token) {
        Ok(a) => a,
        Err(e) => return SyncStatus::err(e.to_string()),
    };

    match sync::sync_once(db, &api) {
        Ok(r) => SyncStatus {
            ok: true,
            message: "同步完成".into(),
            pushed: r.pushed,
            pulled: r.pulled,
            conflicts: r.conflicts,
        },
        Err(e) => SyncStatus::err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    use messagenote_server::{AppState, Store};

    const TOKEN: &str = "worker-test-token-0123456789abcdefghijklmn";

    /// 在专用线程 + 专用 tokio runtime 上跑一个真实服务端，绑临时端口。
    ///
    /// 必须分开线程：测试主体用的是**阻塞式** ureq，和 tokio worker
    /// 挤在同一条线程上会互相堵死。
    fn start_server() -> SocketAddr {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("建 tokio runtime");
            rt.block_on(async move {
                let state = Arc::new(AppState {
                    store: Store::in_memory().expect("建服务端库"),
                    token: TOKEN.to_string(),
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("绑定临时端口");
                tx.send(listener.local_addr().expect("读监听地址"))
                    .expect("回传地址");
                messagenote_server::serve(listener, state)
                    .await
                    .expect("服务端异常退出");
            });
        });
        rx.recv().expect("等待服务端就绪")
    }

    fn configured_db(url: &str) -> Arc<Db> {
        let db = Arc::new(db::open_memory("worker-device").expect("建库"));
        {
            let conn = db.conn().unwrap();
            db::set_sync_config(&conn, url, TOKEN).expect("写同步配置");
        }
        db
    }

    /// 在后台线程上跑 `sync_with`，超时就判定失败。
    ///
    /// **刻意不用 `join()`**：真要是有人把数据库锁带进了网络往返，
    /// `sync_with` 会死锁。用 `join()` 的话整个测试进程会挂住，
    /// 而挂住的测试会被当成"跑得慢"；`recv_timeout` 把它变成一次失败。
    fn sync_with_timeout(db: Arc<Db>, limit: Duration) -> SyncStatus {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(sync_with(&db));
        });
        rx.recv_timeout(limit)
            .expect("sync_with 迟迟不返回 —— 极可能是在网络往返期间死锁了")
    }

    #[test]
    fn the_worker_syncs_against_a_real_server() {
        let addr = start_server();
        let db = configured_db(&format!("http://{addr}"));
        {
            let conn = db.conn().unwrap();
            db::append_message(&conn, "由后台工作线程推上去", None).unwrap();
        }

        let status = sync_with_timeout(Arc::clone(&db), Duration::from_secs(15));

        assert!(status.ok, "同步应当成功，实际：{}", status.message);
        assert!(status.pushed > 0, "那条消息应当被推上去");

        // 推上去之后 dirty 必须清掉，否则下一轮会重复推同一条
        let conn = db.conn().unwrap();
        let dirty: i64 = conn
            .query_row("SELECT COUNT(*) FROM message WHERE dirty = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(dirty, 0, "同步完成后不该残留待上传的行");
    }

    #[test]
    fn an_unconfigured_server_is_reported_clearly() {
        let db = Arc::new(db::open_memory("worker-unconfigured").unwrap());
        let status = sync_with_timeout(db, Duration::from_secs(5));

        assert!(!status.ok);
        assert!(
            status.message.contains("尚未配置"),
            "没配服务端时该说清楚，实际：{}",
            status.message
        );
    }

    /// **生产路径上的锁纪律。**
    ///
    /// `sync::tests` 已经用内存假服务端验证过 `sync_once` 本身不持锁；
    /// 这里验证的是真正跑在应用里的那一条 —— `sync_with` 比它多做了
    /// "读配置"和"建 HTTP 客户端"两件事，而读配置是要持锁的。
    /// 锁只要没在走网络之前放开，用户打字就会卡住整个 HTTP 往返
    /// （连接超时 4 秒、整体兜底 30 秒），而且不会有任何报错。
    #[test]
    fn the_database_stays_available_while_the_worker_talks_to_the_network() {
        // 一个"接得上、但永远不回话"的服务端。刻意用真实 TCP：
        // 要的正是真实网络往返所花的那段时间。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
        let addr = listener.local_addr().expect("读监听地址");
        let (connected_tx, connected_rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                let _ = connected_tx.send(());
                // 拖着不回话，做一个稳定的"网络在途"窗口。
                // 客户端最终会超时报错 —— 那不影响本测试，我们关心的只是
                // 这段窗口里数据库还能不能用。
                std::thread::sleep(Duration::from_millis(500));
            }
        });

        let db = configured_db(&format!("http://{addr}"));
        {
            let conn = db.conn().unwrap();
            // 得有东西待上传，工作线程才会真的发起 HTTP 推送
            db::append_message(&conn, "触发一次推送", None).unwrap();
        }

        let db_for_sync = Arc::clone(&db);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(sync_with(&db_for_sync));
        });

        connected_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("后台同步始终没有发起网络请求");

        // 网络正在途中 —— 此刻数据库必须仍然打得开
        let conn = db
            .try_conn()
            .expect("网络往返期间数据库锁被同步线程占住了，界面上的写入会卡住");
        conn.query_row("SELECT COUNT(*) FROM message", [], |r| r.get::<_, i64>(0))
            .expect("拿到的连接要真的能用");
        drop(conn);

        // 工作线程最终要能自己走出来（超时报错也算走出来）
        rx.recv_timeout(Duration::from_secs(40))
            .expect("同步线程卡死了");

        let _ = server.join();
    }
}
