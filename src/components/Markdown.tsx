import { useMemo, useRef, type MouseEvent } from "react";
import { useApi } from "../lib/apiContext";
import { useAttachmentImages } from "../lib/attachmentUrl";
import { renderMarkdownHtml } from "../lib/markdownHtml";

/**
 * DOMPurify 允许的 URI 协议白名单和渲染本体都在 `lib/markdownHtml.ts` ——
 * 剪贴板的富文本份（`richCopy.ts`）用的是同一份，别在这里再抄一遍。
 *
 * 图片是特殊的：正文里只有 `attachment:<sha256>` 这个引用，真正的字节要
 * 异步去取。取字节的事不在这里做 —— 它需要一份跨组件共享的缓存，交给
 * `useAttachmentImages` 在同一个容器上批量完成。见那边的说明。
 */
export function Markdown({ source }: { source: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const { desktop } = useApi();

  const html = useMemo(() => renderMarkdownHtml(source), [source]);

  /**
   * 外链不能让 WebView 自己导航。
   *
   * 桌面端没有后退键：点一下链接，整个应用就被换成了那个网页，出不来。
   * 所以 http/https 的链接在这里拦下来，交给系统浏览器（桌面端走
   * `desktop.openExternal`，见那边的说明）；网页端开新标签 —— 把应用
   * 自己留在原地，而不是跳走。
   *
   * 其它协议不碰：`attachment:` 链接已被 `useAttachmentImages` 换成带
   * `download` 的 blob URL，页内锚点让浏览器按原生行为走。mailto 在网页端
   * 也是原生的；桌面端实测（2026-10-08，WebView2）是静默无操作 —— 想让它
   * 唤起邮件客户端的话，把拦截正则放宽到 mailto 并在 opener 作用域里补
   * 该协议即可，先用最小面发布。中键（auxclick，click 事件不含它）两边都
   * 不拦：网页端原生开新标签，桌面端未实测（推断是无操作）。
   */
  function handleLinkClick(e: MouseEvent<HTMLDivElement>) {
    const anchor = (e.target as Element).closest("a[href]");
    if (!anchor) return;
    // 读原始属性而不是 `anchor.href`：后者是**解析后的绝对 URL**，会把 `#锚点`
    // 补成当前页面地址、再被下面的正则误拦进系统浏览器。
    const href = anchor.getAttribute("href") ?? "";
    if (!/^https?:/i.test(href)) return;
    e.preventDefault();
    if (desktop) {
      // 失败要留痕（哪怕只在 devtools 里可见）：openUrl 被权限拒绝或系统打开
      // 失败时如果用 `void` 静默吞掉，用户看到的就是"点了没反应"，
      // 除了重启应用没有任何线索 —— 实机调试时踩过。
      desktop.openExternal(href).catch((err) => console.error("打开外链失败：", err));
    } else window.open(href, "_blank", "noopener,noreferrer");
  }

  // 在 layout effect 里改写 DOM，而不是等一次 paint —— 否则正文里那个
  // `attachment:` 开头的 src 会先被浏览器画成一个破图，然后才被换掉。
  useAttachmentImages(ref, html);

  return (
    <div
      className="md"
      ref={ref}
      onClick={handleLinkClick}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
