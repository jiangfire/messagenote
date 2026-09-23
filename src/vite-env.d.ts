/// <reference types="vite/client" />

// 这个文件让 TypeScript 认识 Vite 的资源导入（如 `import "./styles.css"`）
// 以及 `import.meta.env` 上的内置变量。
// 少了它，tsconfig 里的 noUncheckedSideEffectImports 会把 CSS 导入报成错误。
