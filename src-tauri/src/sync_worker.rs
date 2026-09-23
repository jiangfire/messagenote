//! 后台自动同步。
//!
//! 为什么必须是后台线程，而不是"点一下同步一次"：多设备同步要真正有用，
//! **必须是自动的**。需要用户记得手动触发同步的方案等于没有同步 ——
//! 你永远不会在手机上想起来点它。
//!
//! 已知限制（S1）：`sync_once` 全程持有数据库锁，所以网络往返期间界面上
//! 的写操作会短暂阻塞。连接超时压到 4 秒就是为了把最坏窗口限制住。
//! 彻底的做法是把"读本地 → 走网络 → 写本地"拆成三段、网络期间不持锁，
//! 留到接 Web 端时一并做。

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
    let db = app.state::<Db>();
    let conn = match db.conn() {
        Ok(c) => c,
        Err(e) => return SyncStatus::err(e.to_string()),
    };

    let cfg = match db::get_sync_config(&conn) {
        Ok(c) => c,
        Err(e) => return SyncStatus::err(e.to_string()),
    };
    if cfg.url.is_empty() || cfg.token.is_empty() {
        return SyncStatus::err("尚未配置同步服务端".into());
    }

    let api = match HttpServerApi::new(&cfg.url, &cfg.token) {
        Ok(a) => a,
        Err(e) => return SyncStatus::err(e.to_string()),
    };

    match sync::sync_once(&conn, &api) {
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
