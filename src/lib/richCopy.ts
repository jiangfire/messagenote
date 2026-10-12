import type { NoteApi } from "./apiContext";
import { ATTACHMENT_SCHEME } from "./attachmentRef";
import { renderMarkdownHtml } from "./markdownHtml";

/**
 * 「复制这一条」的落剪贴板实现。
 *
 * 剪贴板里同时放**两份**：
 *
 * - `text/plain` = 正文原文。和输入框里的字一字不差，贴到记事本、聊天框
 *   就是当初写下的内容 —— 这是 2026-10-12 用户拍板的主行为。
 * - `text/html` = 渲染过的 HTML，图片内联成 data URI、标题变成真标题。
 *   Word、邮件、支持富文本的编辑器贴出来的是排版和真图；纯文本目标
 *   根本不读这份，行为不变。
 *
 * 两份是「多给」而不是「换掉」：富文本这条路走不通（明文 HTTP 的网页端
 * 没有 Clipboard API）或者中途失败时，**永远退回纯文本** —— 复制最差的
 * 结果也得是拿到原文，而不是什么都没发生。
 */

/**
 * 把一段文字放进剪贴板，成功返回 true。
 *
 * Clipboard API 只在安全上下文（HTTPS / localhost）里存在，而服务端部署
 * 常常就是明文 HTTP —— 这时候 `navigator.clipboard` 是 undefined，
 * 直接调会抛异常，用户看到的是"点了没反应"。
 *
 * 降级路径用一个屏幕外的 textarea + `execCommand("copy")`：它早就被标记为
 * 废弃，但在不安全的上下文里是唯一还能用的办法。拿不到用户手势之外的
 * 权限时会失败，所以返回 false 而不是假装成功。
 */
async function writeClipboard(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // 落到下面的降级路径
  }

  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    // 放在视口外而不是 display:none —— 隐藏元素无法被选中，命令会失败。
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.top = "0";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

/** 正文里引用的附件 sha。只认小写 hex、长度 64 —— 和库里存的一致。 */
const SHA_IN_HTML = new RegExp(`${ATTACHMENT_SCHEME}([0-9a-f]{64})`, "g");

/**
 * 由字节嗅探 MIME，和 Rust 侧 `messagenote_core::attachment::resolve_mime`
 * 认同一批魔法数（png / jpeg / gif / webp / pdf）。data URI 里写的 mime
 * 就是贴入方渲染的依据，编一个 `.png` 出来只会让打不开的图看起来像本来就坏了，
 * 所以认不出的一律 `application/octet-stream`。
 */
function sniffMime(bytes: Uint8Array): string {
  const b = bytes;
  if (
    b.length >= 8 &&
    b[0] === 0x89 && b[1] === 0x50 && b[2] === 0x4e && b[3] === 0x47 &&
    b[4] === 0x0d && b[5] === 0x0a && b[6] === 0x1a && b[7] === 0x0a
  ) {
    return "image/png";
  }
  if (b.length >= 3 && b[0] === 0xff && b[1] === 0xd8 && b[2] === 0xff) {
    return "image/jpeg";
  }
  if (
    b.length >= 6 &&
    b[0] === 0x47 && b[1] === 0x49 && b[2] === 0x46 && b[3] === 0x38 &&
    (b[4] === 0x37 || b[4] === 0x39) && b[5] === 0x61
  ) {
    return "image/gif";
  }
  if (
    b.length >= 12 &&
    b[0] === 0x52 && b[1] === 0x49 && b[2] === 0x46 && b[3] === 0x46 &&
    b[8] === 0x57 && b[9] === 0x45 && b[10] === 0x42 && b[11] === 0x50
  ) {
    return "image/webp";
  }
  if (b.length >= 5 && b[0] === 0x25 && b[1] === 0x50 && b[2] === 0x44 && b[3] === 0x46 && b[4] === 0x2d) {
    return "application/pdf";
  }
  return "application/octet-stream";
}

/** 浏览器里的 base64：`btoa` 的入参是二进制字符串，大文件得切片拼，不然栈会爆。 */
function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(bin);
}

/**
 * 渲染正文，并把 HTML 里所有 `attachment:<sha>` 换成 data URI。
 *
 * **先消毒、后替换**：消毒白名单认 `attachment:`、刻意不认 `data:`（见
 * `markdownHtml.ts` 的说明），顺序反过来刚内联好的图就会被 DOMPurify
 * 整个删掉。替换只往字符串里放 `data:<嗅探的 mime>;base64,<base64>` ——
 * base64 字母表本身无害，不会让消毒后的 HTML 变回危险品。
 *
 * 取不到字节的 sha（对端还没同步过来）**留着原引用**：和全量导出对缺失
 * 附件是同一个口径 —— 至少诚实地说"这里本来有张图"，而不是编一个死链。
 */
async function htmlWithDataUris(api: NoteApi, body: string): Promise<string> {
  const html = renderMarkdownHtml(body);

  const shas: string[] = [];
  for (const m of html.matchAll(SHA_IN_HTML)) {
    if (!shas.includes(m[1])) shas.push(m[1]);
  }
  if (shas.length === 0) return html;

  const uris = new Map<string, string>();
  await Promise.all(
    shas.map(async (sha) => {
      try {
        // 包一层 `new Uint8Array`：桌面端 IPC 返回的视图可能带 offset，
        // 拷一份保证 base64 编码的就是这段字节本身。
        const bytes = new Uint8Array(await api.readAttachment(sha));
        uris.set(sha, `data:${sniffMime(bytes)};base64,${bytesToBase64(bytes)}`);
      } catch {
        // 字节不在本地：HTML 里留着 attachment: 原引用
      }
    })
  );

  return html.replace(SHA_IN_HTML, (whole, sha: string) => uris.get(sha) ?? whole);
}

/**
 * 复制一条记录：纯文本给原文，富文本给渲染好的 HTML（图片内联）。
 *
 * `ClipboardItem` 的值**可以是 Promise**：整个"渲染 + 取字节"都放进
 * Promise 里，`clipboard.write()` 本身留在点击事件的任务里同步发出 ——
 * 这是各浏览器对"异步内容写剪贴板"要求的姿势，等 await 完再调 write
 * 反而可能被判定为没有用户手势。
 *
 * 富文本任何一步失败都退回纯文本 `writeClipboard`，返回值统一表示
 * "用户至少拿到了原文没有"。
 */
export async function writeMessageClipboard(api: NoteApi, body: string): Promise<boolean> {
  if (navigator.clipboard?.write && typeof ClipboardItem !== "undefined") {
    try {
      const html = htmlWithDataUris(api, body);
      await navigator.clipboard.write([
        new ClipboardItem({
          "text/plain": new Blob([body], { type: "text/plain" }),
          "text/html": html.then((h) => new Blob([h], { type: "text/html" })),
        }),
      ]);
      return true;
    } catch {
      // 落到下面的纯文本降级
    }
  }
  return writeClipboard(body);
}
