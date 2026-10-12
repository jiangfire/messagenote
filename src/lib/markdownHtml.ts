import { marked } from "marked";
import DOMPurify from "dompurify";

/**
 * Markdown 源码 → 消毒后的 HTML 字符串。
 *
 * 单独一个 lib 而不是留在 `Markdown.tsx` 里，是因为这份渲染现在有**两个消费方**：
 * 正文气泡和「复制」写进剪贴板的富文本份（见 `richCopy.ts`）。各写一份的话，
 * 换行规则、消毒白名单早晚在某次改动里悄悄错开 —— 到时候同一条笔记
 * 看起来是一个样、复制出去是另一个样。
 */

marked.setOptions({
  gfm: true,
  // 聊天式记录里换行就是换行，不该像写文档那样要求空行才分段
  breaks: true,
});

/**
 * DOMPurify 允许的 URI 协议白名单。
 *
 * 基线是它自己那条默认正则（`node_modules/dompurify/src/regexp.ts` 里的
 * `IS_ALLOWED_URI`），只在协议那一段**追加** `attachment`：
 *
 *     ...|cid|xmpp|matrix):|...             ← 原样
 *     ...|cid|xmpp|matrix|attachment):|...   ← 现在
 *
 * 为什么要自己传：DOMPurify 的 URI 白名单**只认协议名**，它不认识
 * `attachment:`，于是会把 `<img src="attachment:...">` 的 `src` 整个删掉 ——
 * 表现是正文里图片位置空空如也，而控制台一声不响。这是这个功能最容易
 * 卡住的一步。
 *
 * 必须整条重写，是因为 DOMPurify **没有**"追加一个协议"的配置项：
 * `ADD_URI_SAFE_ATTR` 之类管的是属性名，不是协议。所以这里把默认正则抄一份、
 * 只加一个词。抄写是有意的 —— 换成自己手写的宽松版本（比如
 * `/(attachment|https?):/`）等于顺手把 `data:`、`blob:` 那一堆也放开了，
 * 那是另一个安全问题。
 */
export const ALLOWED_URI_REGEXP =
  /^(?:(?:(?:f|ht)tps?|mailto|tel|callto|sms|cid|xmpp|matrix|attachment):|[^a-z]|[a-z+.\-]+(?:[^a-z+.\-:]|$))/i;

/**
 * 渲染并消毒。
 *
 * 必须过一道 DOMPurify：笔记内容虽然是自己写的，但**粘贴**进来的内容
 * 可能夹带 HTML/脚本，而渲染结果是直接 `dangerouslySetInnerHTML` 进
 * WebView 的 —— 在桌面应用里这等价于本地代码执行，不能省。
 * （剪贴板那份一样要过：它会被贴进别的程序里渲染。）
 */
export function renderMarkdownHtml(source: string): string {
  const raw = marked.parse(source, { async: false }) as string;
  return DOMPurify.sanitize(raw, {
    USE_PROFILES: { html: true },
    ALLOWED_URI_REGEXP,
  });
}
