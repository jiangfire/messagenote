import { useEffect, useRef, useState, type ReactNode } from "react";

interface Props {
  /** 按钮上显示的东西，默认是「⋯」。 */
  label?: string;
  title: string;
  /**
   * 菜单内容。
   *
   * 是个函数，参数是 `close` —— **菜单项用完必须调它**。
   */
  children: (close: () => void) => ReactNode;
}

/**
 * 顶栏的「⋯」收纳菜单。
 *
 * ## 为什么要收
 *
 * 顶栏和侧边栏上散着一堆**低频**功能：字号档位、导出全部、同步设置。它们各自
 * 都有用，但使用频率大概是"几个月一次" —— 而它们占着的位置是用户每天都在用的
 * 检索框。代价和收益不匹配，就该收起来。
 *
 * ## 什么时候不该收
 *
 * **出错的指示器不能收。** 同步状态胶囊在失败时必须留在顶栏：那是"我这条到底
 * 存哪儿了"的答案，藏起来就等于让用户自己猜。这条判断写在 App.tsx ——
 * 本组件只管收纳，不判断该不该收纳。
 */
export function OverflowMenu({ label = "⋯", title, children }: Props) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  function close() {
    setOpen(false);
  }

  // 点外面关掉。监听挂在 document 上而不是容器上 ——
  // 菜单是 absolute 定位，展开时会溢出容器，挂在容器上的话点到溢出部分
  // 会被当成"点外面"而立刻关掉。
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) close();
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  // Esc 关掉。键盘用户没有鼠标可点，只靠"点外面"关不掉。
  //
  // `stopPropagation` 是这里的关键，不是"怕打架"：App 那边也监听 Esc 清检索词，
  // 两者都在冒泡链上（document → window）。不拦住的话，按一次 Esc 会
  // **既关菜单又清掉用户正在用的检索词** —— 而那两件事毫无关系。
  //
  // 曾经在 App 那边用"刚关过菜单"这个状态来补救，但 setState 是异步的，
  // 同一个事件循环里读到的还是旧值，拦不住。直接在源头截断才是可靠的。
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      close();
    };
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, [open]);

  return (
    <div className="overflow" ref={root}>
      <button
        type="button"
        className="overflow-btn"
        onClick={() => (open ? close() : setOpen(true))}
        title={title}
        aria-label={title}
        aria-expanded={open}
        aria-haspopup="menu"
      >
        {label}
      </button>

      {open && <div className="overflow-menu" role="menu">{children(close)}</div>}
    </div>
  );
}
