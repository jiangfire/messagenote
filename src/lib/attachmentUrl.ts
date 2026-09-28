import { useLayoutEffect, useMemo, useState, useSyncExternalStore, type RefObject } from "react";
import { useApi, type NoteApi } from "./apiContext";
import { ATTACHMENT_SCHEME } from "./attachmentRef";

/**
 * 附件字节 → 可以塞进 <img src> 的 object URL。
 *
 * ## 为什么要有这一层
 *
 * 正文字节是通过 `api.readAttachment()` 拿的，它异步、可能失败、而且每次
 * 都是一次真实的 IO（桌面端读磁盘，网页端发 HTTP）。但同一张图在时间线里
 * 可能同时出现很多次（一条记录里贴两次、检索结果和时间线各渲染一份、
 * 滚动来回进出视口）。没有缓存的话，每次渲染都要重新取一遍字节、
 * 重新建一遍 URL —— 滚动一下就是几十次请求。
 *
 * ## 缓存的生命周期就是"一个会话"
 *
 * 不在这里做 `URL.revokeObjectURL` 的逐张回收，是**有意的取舍**：
 * 笔记里的图会在整个会话里被反复看到，滚回去就又要用。为了省几 MB 内存
 * 把某个 object URL 提前释放掉，用户看到的就是一张已经渲染好的图突然变成
 * 破图 —— 而"图片会莫名其妙坏掉"是没人能自己排查出来的问题。
 * 页面一关，这些 URL 跟着 document 一起消失，不需要我们操心。
 *
 * 代价是"长时间开着、看过大量图片，这一页的内存会一直涨"。这个产品的时间线
 * 一次也就几百条，图片是笔记里的截图而不是相册，先不做 LRU。
 *
 * ## 并发去重与失败记忆
 *
 * - 同一个 sha 的多个请求合并成一次 `readAttachment`（`pending` 里存的就是
 *   那个 Promise）。
 * - **失败的 sha 会被记住**，不会因为组件重新挂载或滚动再次触发请求。
 *   这条很关键：失败通常意味着"字节确实不在本地"，重试多少次都是一样的
 *   结果，但每次重试都会走一遍 IO，而时间线滚动会非常频繁地触发它。
 */

/** 一个 sha 的解析状态。`url` 和 `failed` 只会有一个起作用。 */
interface Entry {
  /** 拿到字节后建的 object URL。 */
  url?: string;
  /** 取不到（本地没有、网络失败、401…… 都算）。 */
  failed?: boolean;
  /** 正在取的那次请求。 */
  pending?: Promise<void>;
}

const cache = new Map<string, Entry>();

/**
 * 缓存是模块级而不是放在 hook 里，因为它必须**跨组件**共享：
 * 时间线、检索结果、同一条记录里贴了两次的图，引用的都是同一份字节。
 *
 * `NoteApi` 由调用方每次传进来（`useApi()` 注入的那个），这里不自己 import ——
 * 共享组件树里一旦直接 import `lib/api`，网页端 bundle 就会混进 Tauri，
 * 有 `scripts/check-web-bundle.mjs` 专门盯着这条线。
 */

/**
 * 谁在等结果。
 *
 * 一个全局的订阅集合，而不是"按 sha 分桶"：分桶看起来更精细，但只要有一条
 * 通知没送到（通知可能在订阅建立之前就发出去了，见 `useAttachmentImages`
 * 那段注释），对应的图就永远不显示，而且不报任何错。宁可多叫醒几个节点
 * ——每个节点只是重扫一遍自己那棵小树，代价是几次 querySelectorAll。
 */
const listeners = new Set<() => void>();

/** 有字节到手，或者确定取不到了。 */
function notify() {
  for (const l of [...listeners]) l();
}

/** 已经可用的 URL；没有就是 null（没取到、取不到、或还没开始取）。 */
export function getAttachmentUrl(sha256: string): string | null {
  return cache.get(sha256)?.url ?? null;
}

/** 这个 sha 已经确定取不到了。界面据此换成占位块，而不是留个破图。 */
export function isAttachmentUnavailable(sha256: string): boolean {
  return cache.get(sha256)?.failed === true;
}

/** 确保这个 sha 正在被取。已经在取 / 已经成功 / 已经失败都直接返回。 */
export function ensureAttachment(api: NoteApi, sha256: string): boolean {
  const entry = cache.get(sha256);
  if (entry && (entry.pending || entry.url || entry.failed)) return false;

  const next: Entry = {};
  cache.set(sha256, next);

  next.pending = api
    .readAttachment(sha256)
    .then((bytes) => {
      // `new Uint8Array(bytes)` 而不是直接 `new Blob([bytes])`：
      // 桌面端返回的 ArrayBuffer 有可能带 offset（Tauri 的 IPC 缓冲复用），
      // 包一层视图能保证 Blob 拿到的就是这段字节本身。
      next.url = URL.createObjectURL(new Blob([new Uint8Array(bytes)]));
    })
    .catch(() => {
      next.failed = true;
    })
    .finally(() => {
      delete next.pending;
      notify();
    });

  return true;
}

/** 换数据源时清空（现在的入口用不到，但留个明确的收口，别让人去手改 Map）。 */
export function clearAttachmentCache() {
  for (const entry of cache.values()) {
    if (entry.url) URL.revokeObjectURL(entry.url);
  }
  cache.clear();
  notify();
}

/**
 * 订阅"缓存变了"。模块级函数，引用永远稳定，
 * 所以不需要 `useCallback`，也不会让 `useSyncExternalStore` 反复退订重订。
 */
function subscribe(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => {
    listeners.delete(onChange);
  };
}

/**
 * 单个附件的 URL（还没取到就是 null）。
 *
 * 正文渲染不走这里（一张图一个 hook 在循环里是调不了的），它是给**别处**
 * 需要拿一张图当 `<img src>` 用的地方准备的，比如以后的图片查看器。
 *
 * 这里用 `useSyncExternalStore` 是安全的：它只是在字节到手的**之后**某次
 * 通知里补一次重渲染，就算某一次通知没赶上（订阅建立之前就发了），
 * 挂载时读到的也是缓存当前值，最坏情况是晚一拍。正文那条路不能这么随意，
 * 见 `useAttachmentImages`。
 */
export function useAttachmentUrl(sha: string): string | null {
  const { api } = useApi();

  useLayoutEffect(() => {
    if (sha) ensureAttachment(api, sha);
  }, [api, sha]);

  useSyncExternalStore(subscribe, () => getAttachmentUrl(sha));

  return getAttachmentUrl(sha);
}

/**
 * 把一段正文里**所有** `attachment:<sha>` 的图片解析并写回 DOM。
 *
 * ## 为什么走"批量解析 + 直接改 DOM"，而不是每个 `<img>` 一个组件
 *
 * 正文是 `marked` 出来的 HTML 字符串，经 DOMPurify 消毒后由
 * `dangerouslySetInnerHTML` 交给 React。React **完全不管**这棵子树，
 * 想把里面某个 `<img>` 换成 React 组件，就得把 HTML 字符串切开、
 * 让文本节点和组件交替渲染 —— 不仅要处理"切在哪个位置"（元素嵌套、
 * 属性顺序都没保证），还会丢掉"消毒后的字符串就是最终 DOM"这个简单前提。
 *
 * 而 hook 不能在循环里调用，一张图一个 `useAttachmentUrl` 也走不通。
 *
 * 所以这里选了另一条：容器级拿到正文里所有 sha → 批量发起解析 →
 * 把同一个 sha 对应的 `<img src>` 全部改写成 object URL。它和后端的存储
 * 形态是同一个思路（内容寻址：一个 sha 对应一份字节），改的是属性而不是
 * 结构，和 `dangerouslySetInnerHTML` 的协作面最小。
 *
 * ## 通知可能赶不上订阅，所以这里不依赖"收到通知"这件事
 *
 * 踩过的坑值得写下来：`readAttachment` 的 Promise 在**微任务**里就 resolve
 * 了，而 React 的 `useSyncExternalStore` 要到**被动 effect** 阶段才调用
 * subscribe —— 微任务一定跑在被动 effect 之前。于是"字节到手"那次通知发出去
 * 时还没有人订阅，等订阅挂上，缓存里早就是最新值，React 再也等不到"值变了"，
 * 图片就一直停在原始 src 上，而且不报任何错。
 *
 * 所以这里做两件事，缺一不可：
 *
 * 1. 订阅在 **layout effect** 里挂（和发起请求同一个阶段，一定早于任何微任务）；
 * 2. 每次收到通知就 `bump()` 一下计数器，让 effect 无条件重跑一遍重新扫 DOM。
 *    即使某次通知没赶上，`bump` 也会让"重新扫一遍"这件事必然发生一次 ——
 *    扫描本身是幂等的，多扫几次没有副作用。
 *
 * ## 一个必须自己处理的边界
 *
 * 正文里的 `<img src="attachment:...">` 在被改写**之前**不能就那样留在
 * DOM 里 —— 那是一个解析不了的地址，浏览器会给它画一个破图。所以样式里
 * 有一条：src 还是 `attachment:` 开头的图先藏起来，等真正的字节到了再露出来。
 */
export function useAttachmentImages(
  container: RefObject<HTMLElement | null>,
  /** 渲染出来的 HTML 字符串。它一变，DOM 就是新的，得整套重新绑一遍。 */
  html: string
) {
  const { api } = useApi();
  /** 收到一次"缓存变了"就 +1，用来把下面的 effect 再跑一遍。 */
  const [epoch, bump] = useState(0);
  // `epoch` 唯一的用途就是当依赖，但 `noUnusedLocals` 不读依赖数组。
  void epoch;

  // 这段正文引用了哪些附件（去重、保持出现顺序）。
  const shaList = useMemo(() => {
    const found: string[] = [];
    for (const m of html.matchAll(ATTACHMENT_IN_HTML)) {
      if (!found.includes(m[1])) found.push(m[1]);
    }
    return found;
  }, [html]);

  useLayoutEffect(() => {
    const root = container.current;
    if (!root) return;

    // 先挂订阅，再发起请求：通知是在微任务里发的，而这里还在同一次提交的
    // layout 阶段 —— 顺序反过来就会漏掉第一批结果。
    const unsubscribe = subscribe(() => bump((n) => n + 1));

    for (const img of root.querySelectorAll<HTMLImageElement>("img")) {
      const src = img.getAttribute("src");
      const sha = src?.startsWith(ATTACHMENT_SCHEME)
        ? src.slice(ATTACHMENT_SCHEME.length)
        : // src 已经被改写成 object URL 的节点，靠这个属性认出它原来是谁
          img.getAttribute("data-md-attachment");
      if (!sha) continue;

      // 留个印记，重渲染后（`dangerouslySetInnerHTML` 会把 src 还原）还认得出。
      img.setAttribute("data-md-attachment", sha);

      const url = getAttachmentUrl(sha);
      if (url) {
        if (img.getAttribute("src") !== url) img.setAttribute("src", url);
        continue;
      }

      if (isAttachmentUnavailable(sha)) {
        // 取不到字节：换成占位块，而不是留一个永远显示不出来的洞。
        img.replaceWith(placeholderFor(img));
        continue;
      }

      // 还没结果：发起请求，等 notify → bump → 本 effect 重跑。
      ensureAttachment(api, sha);
    }

    return unsubscribe;
  }, [api, container, html, shaList, epoch]);
}

/**
 * 在渲染出来的 HTML 里找 `attachment:<sha64>`。
 *
 * 精度要求不高：多认一个（比如出现在代码块里）只是多读一次本地文件，
 * 漏掉一个才是图片永远不显示。
 */
const ATTACHMENT_IN_HTML = /attachment:([0-9a-f]{64})/g;

/**
 * 取不到字节时的占位块。
 *
 * 把 alt 文字带上：一条记录里常常有好几张图，只写"取不到"的话，
 * 用户不知道丢的是哪一张。
 */
function placeholderFor(img: HTMLImageElement): HTMLElement {
  const box = document.createElement("span");
  box.className = "md-img-placeholder";
  // 用 textContent 而不是拼 HTML：alt 来自笔记正文，是可以由用户随便写的。
  box.textContent = img.alt ? `图片还没下载下来 · ${img.alt}` : "图片还没下载下来";
  return box;
}
