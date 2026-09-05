/**
 * DocMarkdownView — doc 文档只读渲染（预览模式）。
 *
 * 与 `MarkdownPreviewView` 同一渲染栈（react-markdown + remark-gfm +
 * rehype-raw + CodeBlock + 表格滚动 override + prose 样式 token），保证
 * 视觉一致。区别：doc 是独立领域（无 workspace 文件上下文），内链/资源
 * 不做跨工作区解析 —— http(s)/mailto 正常链接，其余相对路径原样渲染。
 */

import { Children, isValidElement, type AnchorHTMLAttributes, type ReactElement, type ReactNode, type TableHTMLAttributes } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeRaw from "rehype-raw";
import { CodeBlock } from "../../components/chat/CodeBlock";
import { cn } from "../../lib/utils";

/** 与 MarkdownPreviewView 对齐的 ReactMarkdown components override */
const markdownComponents = {
  pre: ({ children }: { children?: ReactNode }) => {
    const childArray = Children.toArray(children);
    const codeEl = childArray.find(
      (child): child is ReactElement<{ className?: string; children?: ReactNode }> =>
        isValidElement(child) && child.type === "code",
    );
    if (codeEl) {
      const { className, children: codeContent } = codeEl.props;
      const language = className?.replace(/^language-/, "") || "";
      const code = Children.toArray(codeContent).join("");
      return <CodeBlock language={language} code={code} />;
    }
    return <pre>{children}</pre>;
  },
  table: ({ children, ...rest }: TableHTMLAttributes<HTMLTableElement>) => (
    <div className="prose-table-scroll">
      <table {...rest}>{children}</table>
    </div>
  ),
  a: (props: AnchorHTMLAttributes<HTMLAnchorElement>) => (
    <a {...props} target="_blank" rel="noreferrer" />
  ),
};

export function DocMarkdownView({ content }: { content: string }) {
  return (
    <div
      className={cn(
        "markdown-preview prose prose-sm prose-zinc max-w-none h-full overflow-y-auto bg-editor-canvas px-5 py-4",
      )}
    >
      <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeRaw]} components={markdownComponents}>
        {content}
      </ReactMarkdown>
    </div>
  );
}
