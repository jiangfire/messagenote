import { useEffect, useState } from "react";
import { useApi } from "../lib/apiContext";
import { errorText } from "../lib/errors";
import type { UpdateInfo } from "../lib/types";

/**
 * 有新版本时的一条提示。
 *
 * ## 为什么不静默自动更新
 *
 * 装新版本会**重启应用**，而用户可能正在写东西。捕获浮层里那段还没发出去的
 * 草稿只存在内存里（窗口是隐藏而不是销毁，所以它才留得住），一重启就没了。
 * 拿一段可能正在写的笔记去换一个后台升级，不划算 —— 所以这里只提示，
 * 装不装由用户挑时候。
 *
 * ## 查不到更新时为什么不吭声
 *
 * 最常见的原因是"没网"，其次是"这个项目还没发过带更新清单的 release"
 * （GitHub 会回 404）。两种情况用户都做不了什么，弹一个错误只会让人以为坏了。
 */
export function UpdateNotice() {
  const { desktop } = useApi();
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState(false);

  useEffect(() => {
    if (!desktop) return;
    let alive = true;

    // 延后几秒再查：启动那几秒要留给"把界面画出来 + 读出本地数据"，
    // 而检查更新是可以等的。
    const timer = setTimeout(() => {
      desktop
        .checkForUpdate()
        .then((found) => {
          if (alive) setInfo(found);
        })
        .catch(() => {
          // 见上面的说明：静默
        });
    }, 5000);

    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [desktop]);

  if (!desktop || !info || dismissed) return null;

  async function install() {
    if (!desktop || busy) return;
    setBusy(true);
    setError(null);
    try {
      await desktop.installUpdate();
      // 成功的话走不到这一行 —— 进程会被新版本顶掉
    } catch (e) {
      // 失败最可能的原因是签名对不上。那正是签名存在的意义：
      // 更新包是从网上拿的，不校验就等于让任何人给你换一个 exe。
      setError(errorText(e));
      setBusy(false);
    }
  }

  return (
    <div className="notice-bar">
      <span>
        有新版本 <b>{info.version}</b>
        {info.notes ? `：${info.notes.split("\n")[0]}` : ""}
      </span>
      {error && <span className="update-error">{error}</span>}
      <button className="btn" disabled={busy} onClick={() => void install()}>
        {busy ? "正在下载…" : "更新并重启"}
      </button>
      <button
        className="icon-btn"
        onClick={() => setDismissed(true)}
        title="以后再说"
        aria-label="以后再说"
      >
        ×
      </button>
    </div>
  );
}
