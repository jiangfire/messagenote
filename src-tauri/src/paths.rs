//! 数据目录：安装版和便携版走的是**两个不同的地方**。
//!
//! - **安装版**：数据在系统应用数据目录（Windows 上是 `%APPDATA%\com.messagenote.desktop`）
//! - **便携版**：数据在**可执行文件所在的目录**，和 exe 并排
//!
//! 便携版的全部意义就在这条差别上：把整个文件夹拷到 U 盘，数据跟着走，
//! 换台电脑双击就接着用。数据还躺在 `%APPDATA%` 里的程序不叫便携版 ——
//! 那只是个换了名字的安装版：装的人以为数据跟着程序走，实际上拔了 U 盘
//! 就只剩一个空壳，而记录留在了原机器上。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tauri::Manager;

/// 解析出来的数据目录。setup 里定一次，之后所有人从这儿取。
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 当前是不是便携模式。
///
/// 认两种信号，命中任一即为便携：
///
/// 1. **`MESSAGENOTE_PORTABLE` 环境变量非空** —— 手动切换的入口
/// 2. **exe 文件名里带 "portable"** —— 发行版产出的免安装版就长这样：
///    `MessageNote_<版本>_x64-portable.exe`
///
/// 为什么两条都要：文件名那条覆盖了绝大多数人（双击下载下来的那个 exe），
/// 但用户把文件改名成 `MessageNote.exe` 之后，"便携"就**悄悄失效**了 ——
/// 数据又跑回 `%APPDATA%`，而他以为数据还在 U 盘上。
/// 环境变量是那种情况下的救命稻草：设一次，数据就跟着走。
pub fn is_portable() -> bool {
    if portable_by_env(std::env::var("MESSAGENOTE_PORTABLE").ok().as_deref()) {
        return true;
    }
    portable_by_name(exe_file_name().as_deref())
}

/// 环境变量这一条：非空即算便携（`"0"`、`"false"` 也算 —— 它的意思是"我明确要求"，
/// 不是"这个值是不是真的"，写 `MESSAGENOTE_PORTABLE=0` 想关掉便携的人应该
/// 直接把这个变量删掉）。
fn portable_by_env(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.trim().is_empty())
}

/// 文件名这一条：exe 的名字里带 "portable"（不分大小写）。
fn portable_by_name(file_name: Option<&str>) -> bool {
    matches!(file_name, Some(n) if n.to_lowercase().contains("portable"))
}

/// 可执行文件的名字（小写）。
fn exe_file_name() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
}

/// 可执行文件所在的目录。**便携模式的数据目录就是它** —— 和 exe 并排，
/// 而不是塞进什么子目录：用户打开文件夹就该一眼看见自己的数据。
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// 库文件名。两个地方共用一份 —— 免得搬家的时候源和目标对不上。
pub const DB_FILE: &str = "messagenote.sqlite";

/// 解析出来的数据目录，外加"要不要从旧位置搬家"的结论。
pub struct DataDir {
    /// 数据目录。便携版 = 程序所在目录；安装版 = 系统应用数据目录。
    pub dir: PathBuf,
    /// 搬家的结果。安装版永远是 [`Migration::NotNeeded`]。
    pub migration: Migration,
}

/// 便携版第一次启动时，旧数据的搬家结果。
pub enum Migration {
    /// 没什么可搬的：第一次用，或者已经搬过了。
    NotNeeded,
    /// 搬好了。**旧的那份保留没删** —— 见 [`migrate_legacy_db`] 里的理由。
    Moved(PathBuf),
    /// 没搬成。程序会先用一个空的库启动，用户得自己把旧文件拷过去。
    Failed {
        /// 旧数据在哪。
        from: PathBuf,
        /// 失败的原因。
        reason: String,
    },
}

/// 解析数据目录并记下来。
///
/// 返回 `Err` 时**不要自己弹框** —— 调用方负责：那套流程（写日志、弹原生消息框）
/// 在 [`crate::fatal`] 里，而这个模块不该依赖它。
pub fn resolve(app: &tauri::AppHandle) -> Result<DataDir, String> {
    let (dir, migration) = if is_portable() {
        let dir = exe_dir().ok_or_else(|| "拿不到可执行文件所在目录".to_string())?;
        let migration = migrate_legacy_db(app, &dir);
        (dir, migration)
    } else {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("无法定位应用数据目录：{e}"))?;
        (dir, Migration::NotNeeded)
    };
    // set 只在第一次生效；重复解析（比如测试里反复调用）不算错。
    let _ = DATA_DIR.set(dir.clone());
    Ok(DataDir { dir, migration })
}

/// 便携版第一次启动：把旧数据从 `%APPDATA%` 搬过来。
///
/// **为什么必须搬**：1.10 之前的免安装版，数据其实躺在 `%APPDATA%` 里 ——
/// 那时候的"免安装版"并不是真的免安装。改了目录之后如果什么都不做，
/// 用户升级后会看到一个**空的**应用：记录还在老地方，只是界面不再读它了。
/// 那不叫"换了位置"，那叫"数据不见了"。
///
/// 旧的那份**保留不动**（不删用户的文件）：万一搬家搬砸了，老数据还在。
/// 代价是两份数据各走各的 —— 但"多一份"比"丢一份"好。
fn migrate_legacy_db(app: &tauri::AppHandle, dir: &Path) -> Migration {
    let target = dir.join(DB_FILE);
    if target.exists() {
        // 已经搬过了（或者本来就有数据在这）。不重复搬。
        return Migration::NotNeeded;
    }
    let legacy_dir = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            return Migration::Failed {
                from: PathBuf::from("<未知>"),
                reason: format!("找不到旧数据目录：{e}"),
            }
        }
    };
    let legacy = legacy_dir.join(DB_FILE);
    if !legacy.exists() {
        // 第一次用，没有旧数据。
        return Migration::NotNeeded;
    }
    match copy_db(&legacy, &target) {
        Ok(()) => Migration::Moved(legacy),
        Err(reason) => Migration::Failed {
            from: legacy,
            reason,
        },
    }
}

/// 复制一份库。
///
/// 用 `VACUUM INTO` 而不是拷文件：库开着 WAL，**最近的写入可能还没落进主库
/// 文件** —— 直接拷 `.sqlite` 会丢掉最近那几条（用户那边 WAL 有 4MB，
/// 主库文件才 2MB，差的那部分全在 WAL 里）。`VACUUM INTO` 走的是 SQLite
/// 自己的备份路径，拿到的是**一致的一份**，而且顺手把 WAL 折进主库，
/// 拷出来是干净的单文件。
fn copy_db(from: &Path, to: &Path) -> Result<(), String> {
    // **读写**打开：只读连接碰上没有 `-shm` 的 WAL 库，恢复不了 WAL
    // （SQLITE_CANTOPEN），而 WAL 里的数据正是最不能丢的那部分。
    let src = rusqlite::Connection::open(from).map_err(|e| format!("打不开旧数据：{e}"))?;
    // 等一下别的进程：用户很可能正开着旧版，库文件是锁着的。
    src.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| format!("设置等待超时失败：{e}"))?;
    let dest = to.to_string_lossy().into_owned();
    src.execute("VACUUM INTO ?1", [dest.as_str()])
        .map_err(|e| format!("复制旧数据失败：{e}"))?;
    Ok(())
}

/// 已经解析出来的数据目录。
///
/// 没解析过就是 `None` —— 那种情况只发生在启动最早期、数据目录都还没定下来
/// 就已经失败的时候（见 [`crate::fatal`] 里的兜底）。
pub fn data_dir() -> Option<&'static Path> {
    DATA_DIR.get().map(|p| p.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_name_with_portable_means_portable() {
        assert!(portable_by_name(Some(
            "messagenote_1.10.0_x64-portable.exe"
        )));
        assert!(portable_by_name(Some("MessageNote-Portable.exe")));
    }

    #[test]
    fn ordinary_exe_is_not_portable() {
        assert!(!portable_by_name(Some("messagenote.exe")));
        assert!(!portable_by_name(Some("MessageNote.exe")));
        assert!(!portable_by_name(None));
    }

    #[test]
    fn env_var_opts_in() {
        assert!(portable_by_env(Some("1")));
        assert!(portable_by_env(Some("0")));
        assert!(!portable_by_env(None));
        assert!(!portable_by_env(Some("")));
        // 空白等于没设：和 MESSAGENOTE_DB 那条逃生口的判断口径保持一致
        assert!(!portable_by_env(Some("   ")));
    }

    #[test]
    fn exe_dir_is_available() {
        // 测试二进制自己就在某个目录里，拿得到目录说明 current_exe 这条路是通的
        assert!(exe_dir().is_some());
    }

    #[test]
    fn copy_db_keeps_rows_that_are_still_in_the_wal() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let dir = std::env::temp_dir().join(format!(
            "messagenote-paths-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.sqlite");
        let dst = dir.join("dst.sqlite");

        // 造一个**数据还在 WAL 里**的库：主库文件里几乎什么都没有。
        // 这正是"只拷 .sqlite 文件"会把数据丢掉的场景 ——
        // 用户那边 WAL 有 4MB，主库才 2MB。
        let conn = rusqlite::Connection::open(&src).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE t(x INTEGER);
             INSERT INTO t VALUES (42);",
        )
        .unwrap();
        let wal = dir.join("src.sqlite-wal");
        assert!(wal.exists(), "数据应当还在 WAL 里");

        // 连接**开着**的时候拷 —— 这才是最难的场景（另一个进程正用着这个库）。
        copy_db(&src, &dst).expect("复制旧数据");
        drop(conn);

        let got: i64 = rusqlite::Connection::open(&dst)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(got, 1, "复制出来的库必须带着 WAL 里的那一行");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
