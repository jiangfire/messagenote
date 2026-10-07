//! 启动阶段的错误上报。
//!
//! ## 为什么需要这个模块
//!
//! release 构建带着 `windows_subsystem = "windows"`，**没有控制台**。
//! 而 Tauri 在 `setup` 返回 `Err` 时会**直接 panic**（不是把错误交回 `run()`），
//! 所以 `panic!` / `eprintln!` / `.expect()` 的输出哪儿都不会去 ——
//! 用户看到的现象是：双击图标，鼠标转一下，什么都没发生。
//!
//! 偏偏它发生在最需要解释的时候：数据库损坏、磁盘满、目录没权限。
//! 所以启动失败必须**主动弹出来**，并且同时在磁盘上留一份可粘贴的报告。
//!
//! 判据很简单：**凡是"应用等于没启动"的失败，都必须走 [`report`]。**

use std::path::PathBuf;
use std::sync::Mutex;

/// 本次启动期间记下的**非致命**警告。
///
/// 光写日志是不够的：用户不会去翻 `%APPDATA%` 下的 `startup-warnings.log`。
/// 全局快捷键被别的程序占用这种事如果不显示在界面上，用户感受到的只是
/// **"这个软件有时候按快捷键没反应"** —— 一个他永远查不出原因的现象。
static STARTUP_WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 报告一个致命错误：弹原生消息框 + 尽力写日志，然后退出进程。
pub fn report(message: &str) -> ! {
    let full = format!(
        "MessageNote 无法启动。\n\n\
         {message}\n\n\
         ────────────────\n\
         常见原因：\n\
         · 数据目录没有写入权限\n\
         · 磁盘空间不足\n\
         · 数据库文件被其它程序占用或已损坏\n\n\
         日志已尝试写入数据目录下的 startup-error.log。"
    );

    // 先写盘、再弹窗 —— 万一弹窗本身失败（比如没有交互桌面），日志还在
    if let Some(path) = log_path("startup-error.log") {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        if std::fs::write(&path, &full).is_ok() {
            eprintln!("启动失败，日志已写入 {}", path.display());
        }
    }

    show_message_box("MessageNote 启动失败", &full);
    std::process::exit(1);
}

/// 记一条**非致命**警告。
///
/// 有些问题不该拦启动（比如全局快捷键被别的程序占用），但用户仍然需要知道，
/// 否则他会一直以为是应用坏了。写进文件，stderr 上也留一份（debug 构建可见）。
///
/// **追加写。** 原来这里是覆盖写（`fs::write`），理由是"这里的语义是本次启动的
/// 警告，不是累积日志"—— 但那样磁盘上就**永远只剩最后一条**：
/// 启动时记了三条，文件里只有第三条，而用户翻日志想看"刚才都提示了什么"
/// 看到的是残缺的。进程内的 [`STARTUP_WARNINGS`] 是完整的，只有文件不是，
/// 这种"两处都叫日志、内容却不一样"的分裂比缺日志更误导。
///
/// 上限用 64 KB：真跑到这个量说明有东西在疯狂刷屏，再多对排查没有帮助。
/// 写失败一律忽略 —— 记一条警告不该让启动失败。
pub fn note_warning(message: &str) {
    eprintln!("[MessageNote 警告] {message}");

    // 除了日志，还要留给界面 —— 见 `STARTUP_WARNINGS` 上的说明
    if let Ok(mut list) = STARTUP_WARNINGS.lock() {
        list.push(message.to_string());
    }

    let Some(path) = log_path("startup-warnings.log") else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = append_capped(&path, format!("{message}\n"));
}

/// 追加写，并在超过 [`WARNING_LOG_CAP`] 时截断重写（只留最后若干行）。
fn append_capped(path: &std::path::Path, line: String) -> std::io::Result<()> {
    use std::io::Write;

    let too_big = std::fs::metadata(path).map(|m| m.len() > WARNING_LOG_CAP).unwrap_or(false);
    if too_big {
        // 只保留最后 200 行：真跑到 64 KB 说明有东西在疯狂刷屏，
        // 再多对排查也没有帮助，而把整个文件留在那儿只会越来越难翻。
        if let Ok(old) = std::fs::read_to_string(path) {
            let all: Vec<&str> = old.lines().collect();
            let start = all.len().saturating_sub(200);
            let mut f = std::fs::File::create(path)?;
            for l in &all[start..] {
                writeln!(f, "{l}")?;
            }
        }
    }

    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

/// 警告日志的上限。超过就截断成最后 200 行。
const WARNING_LOG_CAP: u64 = 64 * 1024;

/// 取出本次启动记下的警告，供界面显示。
///
/// 前端挂载时**主动读一次**，不能只靠事件推送 —— 这些警告产生于 `setup` 阶段，
/// 那时窗口还没加载完，事件发出去根本没人听。这和后台同步状态是同一个坑
/// （见 `sync_worker::SyncWorker::last` 的注释）。
pub fn startup_warnings() -> Vec<String> {
    STARTUP_WARNINGS
        .lock()
        .map(|list| list.clone())
        .unwrap_or_default()
}

/// 优先放在应用数据目录（跟库在一起，用户找得到）；
/// 那里写不进去就退到临时目录 —— 记日志失败不该把"告诉用户"这件事也丢掉。
///
/// 刻意**不依赖 Tauri 的路径解析**：那个 API 本身就可能正是失败的原因。
fn log_path(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("APPDATA")
        .map(|appdata| PathBuf::from(appdata).join("com.messagenote.desktop"))
        .unwrap_or_else(std::env::temp_dir);
    Some(dir.join(name))
}

#[cfg(target_os = "windows")]
fn show_message_box(title: &str, body: &str) {
    use std::os::windows::ffi::OsStrExt;

    // 直接调 user32，不引第三方依赖 —— 为了一个错误提示框装一个 dialog 插件
    // 并不划算，而且插件本身还要联网拉依赖。
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            u_type: u32,
        ) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    const MB_OK: u32 = 0x0000_0000;
    const MB_ICONERROR: u32 = 0x0000_0010;
    const MB_SETFOREGROUND: u32 = 0x0001_0000;

    let text = wide(body);
    let caption = wide(title);
    // SAFETY: 两个指针都指向以 NUL 结尾、且在调用期间存活的宽字符串。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

/// 非 Windows 平台没有 MessageBoxW，退化成打印 ——
/// 那边的终端不会被隐藏，stderr 本来就看得见。
#[cfg(not(target_os = "windows"))]
fn show_message_box(_title: &str, body: &str) {
    eprintln!("{body}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_are_kept_for_the_ui() {
        note_warning("测试警告甲");
        note_warning("测试警告乙");

        // 这里的是**进程级**全局状态，别的测试也可能往里写，
        // 所以只断言"包含"和相对顺序，不断言总数。
        let list = startup_warnings();
        let a = list
            .iter()
            .position(|w| w == "测试警告甲")
            .expect("甲应当在");
        let b = list
            .iter()
            .position(|w| w == "测试警告乙")
            .expect("乙应当在");
        assert!(a < b, "警告应当按发生顺序保留：{list:?}");
    }
}
