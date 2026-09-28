import { listen } from "@tauri-apps/api/event";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { api as commands } from "./api";
import type { DesktopApi, NoteApi } from "./apiContext";
import type { SyncStatus } from "./types";

/** 后台同步线程推上来的状态事件名，与 `sync_worker.rs` 的 `STATUS_EVENT` 一致。 */
const SYNC_STATUS_EVENT = "sync://status";

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
    await relaunch();
  },
};
