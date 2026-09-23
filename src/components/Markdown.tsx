import { useMemo } from "react";
import { marked } from "marked";
import DOMPurify from "dompurify";

marked.setOptions({
  gfm: true,
  // 聊天式记录里换行就是换行，不该像写文档那样要求空行才分段
  breaks: true,
});

/**
 * Markdown 渲染。
 *
 * 这里必须过一道 DOMPurify：笔记内容虽然是自己写的，但**粘贴**进来的内容
 * 可能夹带 HTML/脚本，而渲染结果是直接 `dangerouslySetInnerHTML` 进
 * WebView 的 —— 在桌面应用里这等价于本地代码执行，不能省。
 */
export function Markdown({ source }: { source: string }) {
  const html = useMemo(() => {
    const raw = marked.parse(source, { async: false }) as string;
    return DOMPurify.sanitize(raw, {
      USE_PROFILES: { html: true },
      ADD_ATTR: ["target", "rel"],
    });
  }, [source]);

  return <div className="md" dangerouslySetInnerHTML={{ __html: html }} />;
}
