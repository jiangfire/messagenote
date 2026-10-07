//! MessageNote —— 聊天框式本地优先笔记。
//!
//! P0 的产品原则：
//! - **捕获不做决策**：全局快捷键唤起浮层，输入即落收件箱，不选频道、不打标签。
//! - **组织发生在事后**：频道归档、标签、检索都是整理动作。
//! - **常驻而不是"打开"**：关窗只是隐藏，托盘一直在，随时能写。
//!
//! 与检索分词、时钟、同步合并相关的逻辑住在 `messagenote-core`，
//! 那里是桌面端和服务端的共同依赖。这里只保留"桌面外壳"该管的事。

// 这些模块对外可见，是为了让 `tests/sync_e2e.rs` 能用**真实的**本地库和
// 同步引擎去对接一个真实跑起来的 axum 服务端。
pub mod db;
pub mod error;
pub mod export;
pub mod http;
pub mod sync;

mod commands;
mod fatal;
mod llm;
mod sse;
mod suggest;
mod sync_worker;

use std::sync::atomic::{AtomicI64, Ordering};

use messagenote_core::now_ms;
use tauri::{AppHandle, Manager, WebviewWindow, WindowEvent};
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};

/// 主窗口：完整的时间流与整理界面。
const MAIN_LABEL: &str = "main";
/// 捕获浮层：只有一个输入框的置顶小窗。
const CAPTURE_LABEL: &str = "capture";

/// 浮层刚 `show()` 之后的一小段时间内会收到"伪失焦"事件
/// （窗口还在获取焦点的过程中就会报一次失去焦点）。
/// 不加这个宽限期的话，浮层会一闪而过 —— 这是自绘置顶窗口最典型的坑。
const CAPTURE_FOCUS_GRACE_MS: i64 = 400;

/// 最近一次显示浮层的时间戳（毫秒）。
static CAPTURE_SHOWN_AT: AtomicI64 = AtomicI64::new(0);

// ---------------------------------------------------------------- 主窗口

/// 把主窗口拿到前面来。
///
/// 「第二个实例启动」和「托盘唤起」都要这个动作，所以只留一份实现 ——
/// 两份迟早会分叉成"从托盘点出来是还原过的、从图标点出来还是最小化的"。
fn show_main(app: &AppHandle) {
    let Some(win) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    let _ = win.show();
    let _ = win.unminimize();
    let _ = win.set_focus();
}

/// 显示/隐藏主窗口。
///
/// 「已经在前台就隐藏」这个行为是刻意的：托盘的单击既当"唤起"又当"收起"。
///
/// 注意它和 [`show_main`] 的区别：这里是**切换**，用户主动点托盘才用它。
/// 第二个实例启动时绝不能用它 —— 那会把用户已经开着的窗口藏起来。
fn toggle_main(app: &AppHandle) {
    let Some(win) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    if win.is_visible().unwrap_or(false) && win.is_focused().unwrap_or(false) {
        let _ = win.hide();
    } else {
        show_main(app);
    }
}

// ---------------------------------------------------------------- 捕获浮层

/// 把浮层摆到屏幕水平居中、垂直约 22% 处。
///
/// 不放在正中：那是阅读时视线停留的位置，浮层挡在那里会打断思路。
/// 偏上一点既在余光范围内，又不遮住正在看的内容。
fn position_capture(win: &WebviewWindow) {
    let monitor = win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| win.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return;
    };
    let Ok(size) = win.outer_size() else {
        return;
    };

    let msize = monitor.size();
    let mpos = monitor.position();
    let x = mpos.x + (msize.width as i32 - size.width as i32) / 2;
    let y = mpos.y + (msize.height as f64 * 0.22) as i32;
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
}

fn show_capture(app: &AppHandle) {
    let Some(win) = app.get_webview_window(CAPTURE_LABEL) else {
        return;
    };
    position_capture(&win);
    let _ = win.show();
    let _ = win.set_focus();
    CAPTURE_SHOWN_AT.store(now_ms(), Ordering::SeqCst);
}

fn hide_capture(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(CAPTURE_LABEL) {
        let _ = win.hide();
    }
}

fn toggle_capture(app: &AppHandle) {
    let Some(win) = app.get_webview_window(CAPTURE_LABEL) else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    } else {
        show_capture(app);
    }
}

// ---------------------------------------------------------------- 托盘

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let capture = MenuItem::with_id(app, "capture", "快速记录…", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&capture, &show, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("MessageNote —— Ctrl+Shift+Space 快速记录")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "capture" => show_capture(app),
            "show" => toggle_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键单击托盘图标 = 打开/收起主窗口
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_main(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

// ---------------------------------------------------------------- 装配

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // ---- 单实例 ----
    //
    // 用户重复点图标（或开机自启之后又手动点一次）时，**第二个进程只该把已经
    // 开着的那个拿到前面来，然后自己退出**。没有这一层，第二个进程会抢同一个
    // `messagenote.sqlite`、再挂一个托盘图标，用户看到的是"点了没反应" ——
    // 而实际上多了一个常驻进程。
    //
    // **逃生口是 `MESSAGENOTE_DB`**：设了它说明调用方知道自己在干什么
    // （见 setup 里对它的说明，主要用途就是同机跑两个实例验证同步）。
    // 少了这个例外，"验证同步"这件事在开发机上就没法做了。
    let single_instance = std::env::var("MESSAGENOTE_DB").map_or(true, |p| p.trim().is_empty());

    // 这个插件**必须第一个注册**（官方文档的要求）：它在 setup 阶段就要决定
    // "我是不是第一个实例"，排到别的插件后面会发现不了已经存在的那个。
    let builder = tauri::Builder::default();
    let builder = if single_instance {
        builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 第一个实例收到的回调。用户按图标时期待的就是"把那个拿来"，
            // 所以这里是 show 而不是 toggle —— toggle 可能把窗口藏起来。
            show_main(app);
        }))
    } else {
        builder
    };

    let outcome = builder
        // 自动更新。检查/下载/校验签名都在 Rust 侧，界面只负责问用户一句。
        // 装完之后的重启走 `commands::relaunch`（它要先把单实例占的名字放掉），
        // 所以这里不需要 tauri-plugin-process。
        .plugin(tauri_plugin_updater::Builder::new().build())
        // 导出时选目标目录。权限只给主窗口，见 capabilities/dialog.json。
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    // 只响应按下，否则一次按键会触发 press + release 两次切换
                    if event.state() == ShortcutState::Pressed {
                        toggle_capture(app);
                    }
                })
                .build(),
        )
        .setup(|app| {
            // ⚠️ 这里的错误**不能**用 `?` 往外抛。
            //
            // Tauri 在 setup 返回 Err 时会**直接 panic**（见 tauri/src/app.rs），
            // 而不是把错误交回 `run()`。release 构建带
            // `windows_subsystem = "windows"`、没有控制台，panic 信息哪儿都不去 ——
            // 用户看到的现象是"双击图标，什么都没发生"，没有任何线索。
            //
            // 所以每个致命步骤都自己兜住、自己报（弹框 + 写日志）。
            // 这不是防御性编程，是发布版唯一能让用户看见的通道。

            // ---- 数据库 ----
            let dir = match app.path().app_data_dir() {
                Ok(d) => d,
                Err(e) => fatal::report(&format!("无法定位应用数据目录：{e}")),
            };

            // MESSAGENOTE_DB 可以覆盖数据库位置。两个实际用途：
            // 1. 在同一台机器上跑两个实例来验证同步 —— 否则它们会抢同一个库文件
            // 2. 拿一份副本做实验，不碰真实数据
            let db_path = match std::env::var("MESSAGENOTE_DB") {
                Ok(p) if !p.trim().is_empty() => std::path::PathBuf::from(p),
                _ => dir.join("messagenote.sqlite"),
            };
            let db = match db::open(&db_path) {
                Ok(db) => db,
                Err(e) => {
                    fatal::report(&format!("打不开数据库：{e}\n\n路径：{}", db_path.display()))
                }
            };
            app.manage(db);

            // ---- 托盘常驻 ----
            // 托盘失败是致命的：关窗只隐藏，没有托盘就再也叫不回来了。
            if let Err(e) = setup_tray(app.handle()) {
                fatal::report(&format!("创建托盘图标失败：{e}"));
            }

            // ---- 全局快捷键 ----
            // Ctrl+Shift+Space 唤起的是**捕获浮层**而不是主窗口。
            // 这是产品原则在快捷键上的体现：最高频的动作是"记一笔"，
            // 而不是"打开一个应用去翻记录"。
            //
            // 快捷键被别的程序占用是**非致命**的：托盘还在，功能不受损，
            // 只是少了最顺手的那条路径。所以这里只记一笔，不拦启动 ——
            // 但那一笔会显示到界面上（`fatal::startup_warnings`）。
            let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space);
            {
                use tauri_plugin_global_shortcut::GlobalShortcutExt;
                if let Err(e) = app.global_shortcut().register(shortcut) {
                    fatal::note_warning(&format!(
                        "Ctrl+Shift+Space 注册失败（多半已被其它程序占用）：{e}"
                    ));
                }
            }

            // 验证用的注入点：让"启动警告"这条界面路径能被真的看到一次。
            // 正常使用不会设这个变量 —— 它存在的唯一理由是
            // "快捷键被占用"在开发机上没法按需复现。
            if let Ok(msg) = std::env::var("MESSAGENOTE_TEST_WARNING") {
                fatal::note_warning(&msg);
            }

            // ---- 后台自动同步 ----
            // 同步必须是自动的：需要用户记得手动点的同步，等于没有同步。
            sync_worker::spawn(app.handle().clone());

            // ---- 实时推送 ----
            // 服务端一有写入就把同步线程叫醒，不用干等 45 秒。
            // **轮询保留**：这条长连接随时可能被中间某一层悄悄掐掉，
            // 而"掐掉了"和"没有新数据"在客户端看起来一模一样。
            // 推送负责快，轮询负责兜底。
            sse::spawn(app.handle().clone());

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 关窗 = 隐藏。应用常驻托盘，捕获入口永远活着。
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            // 浮层失去焦点就自动收起 —— 点别处即消失，不需要去够 Esc。
            WindowEvent::Focused(false)
                if window.label() == CAPTURE_LABEL
                    && now_ms() - CAPTURE_SHOWN_AT.load(Ordering::SeqCst)
                        > CAPTURE_FOCUS_GRACE_MS =>
            {
                let _ = window.hide();
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_channels,
            commands::create_channel,
            commands::rename_channel,
            commands::delete_channel,
            commands::list_timeline,
            commands::append_message,
            commands::update_message,
            commands::delete_message,
            commands::move_message,
            commands::search_messages,
            commands::list_tags,
            commands::timeline_stats,
            commands::set_message_tags,
            commands::get_sync_config,
            commands::set_sync_config,
            commands::sync_now,
            commands::get_sync_status,
            commands::test_sync_connection,
            commands::hide_capture,
            commands::get_startup_warnings,
            commands::save_attachment,
            commands::read_attachment,
            commands::collect_garbage_attachments,
            commands::reset_upload_flags,
            commands::reset_upload_flags,
            commands::get_llm_config,
            commands::set_llm_config,
            commands::list_tag_suggestions,
            commands::suggest_tags,
            commands::accept_tag_suggestion,
            commands::export_markdown,
            commands::render_message_markdown,
            commands::relaunch,
        ])
        .run(tauri::generate_context!());

    // 这只是兜底：**setup 阶段的失败不会走到这里** —— Tauri 遇到 setup 返回 Err
    // 会直接 panic（见 setup 里的说明），所以那些错误在闭包内部就已经被
    // `fatal::report` 兜住并弹框了。这里覆盖的是其余拿不到界面的启动失败。
    if let Err(err) = outcome {
        fatal::report(&err.to_string());
    }
}
