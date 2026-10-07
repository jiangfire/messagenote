import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { api as commands } from "./api";
import type { DesktopApi, NoteApi } from "./apiContext";
import type { ExportSummary, LlmConfig, SuggestResult, SyncStatus, TagSuggestion } from "./types";

/** 后台同步线程推上来的状态事件名，与 `sync_worker.rs` 的 `STATUS_EVENT` 一致。 */
const SYNC_STATUS_EVENT = "sync://status";

/**
 * 同步**往本地带了新数据**的事件名，与 `sync_worker.rs` 的 `CHANGED_EVENT` 一致。
 *
 * 注意它和上面那个状态事件是两件事：状态每轮都发，这个只在真的拉到了东西
 * （或留下了冲突副本）时才发。
 */
const SYNC_CHANGED_EVENT = "sync://changed";

/**
 * 已经查到、但还没装的那个更新。
 *
 * 为什么留在这里而不是交给界面拿着：`Update` 是插件的一个**句柄**
 * （里面有下载状态），把它传进 React state 既没意义也让 Tauri 的类型
 * 泄漏到共享的界面代码里。界面只需要知道"有新版本、版本号是多少"，
 * 要装的时候回来调 `installUpdate` 就行。
 *
 * 只有一个窗口会用（见 `capabilities/updater.json`），所以这个模块级的
 * 变量不会有两个使用者抢。
 */
let pendingUpdate: Update | null = null;

/**
 * 桌面端实现：走 Tauri `invoke`，就是原来那一份。
 *
 * 不重新包一层薄封装，而是直接把 `api.ts` 那个对象当 `NoteApi` 用 ——
 * 包一层只会多一个需要同步维护的地方，而它多出来的方法（同步相关的）
 * 正好是 `DesktopApi` 要的。
 */
export const tauriApi: NoteApi = commands;

export const tauriDesktop: DesktopApi = {
  hideCapture: () => commands.hideCapture(),
  getSyncConfig: () => commands.getSyncConfig(),
  setSyncConfig: (url, token) => commands.setSyncConfig(url, token),
  syncNow: () => commands.syncNow(),
  getSyncStatus: () => commands.getSyncStatus(),
  testSyncConnection: (url, token) => commands.testSyncConnection(url, token),
  getStartupWarnings: () => commands.getStartupWarnings(),

  onSyncStatus(handler) {
    let unlisten: (() => void) | null = null;
    let cancelled = false;

    void listen<SyncStatus>(SYNC_STATUS_EVENT, (e) => handler(e.payload)).then((un) => {
      // 组件完全可能在 listen 完成**之前**就卸载了。那种情况下这个 unlisten
      // 没人会调用，监听器会一直挂着，每次同步都往一个已卸载的组件里 setState。
      if (cancelled) un();
      else unlisten = un;
    });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  },

  async checkForUpdate() {
    // 换新的之前先把旧句柄放掉 —— `Update` 在 Rust 侧占着一个资源。
    // 不重复检查的话这里不会触发，但"检查两次"是很自然就会被写出来的代码。
    await pendingUpdate?.close();

    pendingUpdate = await check();
    if (!pendingUpdate) return null;
    return {
      version: pendingUpdate.version,
      // 注意名字错位：`latest.json` 里这个字段叫 `notes`，
      // 而插件在 JS 侧把它暴露成 `body`。
      notes: pendingUpdate.body ?? null,
      date: pendingUpdate.date ?? null,
    };
  },

  async installUpdate() {
    if (!pendingUpdate) return;
    // 下载 + **校验签名** + 安装。签名对不上会在这里失败 —— 那正是它存在的
    // 意义：更新包是从网上拿的，没有签名校验就等于让任何人给你换一个 exe。
    await pendingUpdate.downloadAndInstall();
    // Windows 上 NSIS 装完本来也会拉起新版本，但显式重启让行为不依赖安装器。
    // **走自己的命令，不用插件的 relaunch**：那一步要先把单实例插件占的名字
    // 放掉，否则新进程会以为老的还活着，而老的正准备退出 ——
    // 结果是"更新完应用不见了"。见 `commands::relaunch`。
    await commands.relaunch();
  },

  // ------------------------------------------------------------ 导出

  async pickDirectory() {
    const picked = await open({
      directory: true,
      multiple: false,
      title: "导出到哪个目录",
    });
    // 用户取消时插件返回 null。类型上还可能是 `string[]`（multiple 的情况），
    // 这里收窄掉 —— 界面对"没选"和"选了一个"只该有两种反应。
    return typeof picked === "string" ? picked : null;
  },

  exportMarkdown(dir, utcOffsetMinutes, filter) {
    // 参数名是 **camelCase**：Tauri v2 默认把 Rust 那边的 snake_case 转过来，
    // 写 `utc_offset_minutes` 会得到一个"缺少参数"的报错。
    //
    // `filter` 显式传 `null` 而不是省略：全量导出时 Rust 侧收到的是空筛选，
    // 和"这个参数根本没传"在那边是同一个意思，但显式一点，
    // 出错时的现象是明确的。
    return invoke<ExportSummary>("export_markdown", {
      dir,
      utcOffsetMinutes,
      filter: filter ?? null,
    });
  },

  renderMessageMarkdown(id, utcOffsetMinutes) {
    return invoke<string | null>("render_message_markdown", { id, utcOffsetMinutes });
  },

  // ------------------------------------------------------------ 附件维护

  collectGarbageAttachments() {
    return invoke<number>("collect_garbage_attachments");
  },

  resetUploadFlags() {
    return invoke<number>("reset_upload_flags");
  },

  // ------------------------------------------------------------ AI 标签建议

  getLlmConfig() {
    return invoke<LlmConfig>("get_llm_config");
  },

  setLlmConfig(config) {
    return invoke<void>("set_llm_config", { config });
  },

  listTagSuggestions(messageId) {
    return invoke<TagSuggestion[]>("list_tag_suggestions", { messageId });
  },

  suggestTags(messageId) {
    return invoke<SuggestResult>("suggest_tags", { messageId });
  },

  acceptTagSuggestion(messageId, name) {
    return invoke<void>("accept_tag_suggestion", { messageId, name });
  },
};

/**
 * 桌面端的「别的地方改了数据，去重取一次」。
 *
 * 网页端靠它自己那条 SSE 实现（`web/sse.ts`），桌面端靠这个：Rust 侧的 SSE 线程
 * 唤醒同步线程，**等变更真的落库之后**再往界面发一个事件。所以界面重取时数据
 * 一定已经在库里了 —— 不存在"重取早了、然后就没有下一次通知"的竞态。
 *
 * 少了它，界面会**永远不刷新**。这不是理论：2026-09 实机验证时，远端变更
 * 0.28 秒就进了本地库，而时间线上 70 秒都没出现，手动重载才看见。
 * 那条路径只有实机才暴露 —— `cargo test` 验的是 Rust 侧的库，
 * 浏览器 E2E 跑的是网页端（那边这个钩子是有的）。
 */
export function tauriSubscribeChanges(handler: () => void): () => void {
  let unlisten: (() => void) | null = null;
  let cancelled = false;

  void listen(SYNC_CHANGED_EVENT, () => handler()).then((un) => {
    // 和 onSyncStatus 同一个坑：组件可能在 listen 完成**之前**就卸载了。
    if (cancelled) un();
    else unlisten = un;
  });

  return () => {
    cancelled = true;
    unlisten?.();
  };
}
