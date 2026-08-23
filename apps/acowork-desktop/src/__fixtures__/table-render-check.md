<!-- markdownlint-disable MD013 -->
# Markdown 表格列宽回归样本

> 在 file tab 打开本文件（Markdown Preview 模式）应当看到：
>
> 1. 所有表头标题单行不换行（即便只有 2-5 个字符）
> 2. 含超长 URL 的列宽由横向滚动条消化，**不再挤压其他列**
> 3. 短词不会被强制断字（`Status`、`OK`、`Title` 完整保留）
> 4. 表格整体圆角和外边框来自 `.prose-table-scroll` wrapper

## Case 1：长 URL 列 + 短标题列（核心回归）

| Title | URL | OK |
| --- | --- | --- |
| foo | https://example.com/a/very/long/path/that/exceeds/the/container/width/by/a/lot/x/y/z | yes |
| bar | https://example.com/another/long/url | no |
| baz | short | yes |

## Case 2：5 字母标题 vs. 2 字母标题（原 bug 样本）

| Name | State | Code | Total | Avg |
| --- | --- | --- | --- | --- |
| alpha | OK | 200 | 1234 | 6.2 |
| beta | NG | 500 | 5678 | 7.1 |
| ga | OK | 200 | 9 | 0.1 |

## Case 3：纯宽表格（容器装不下，触发横向滚动）

| Method | Endpoint | Payload | Response |
| --- | --- | --- | --- |
| POST | /api/v1/agents/com.acowork.senior-engineer/sessions/sess-abc-12345-xyz-67890/messages | `{ "content": "hello" }` | `{ "id": "msg-uuid-v4-string-format-abcdef0123456789", "status": "ok" }` |
| GET | /api/v1/agents/com.acowork.senior-engineer/sessions/sess-abc-12345-xyz-67890/messages?offset=0&limit=50 | — | `{ "messages": [], "total": 0 }` |
