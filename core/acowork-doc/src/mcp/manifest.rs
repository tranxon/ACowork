//! MCP 工具 manifest（JSON 格式，对应 `/mcp/tools/list` 响应）。
//!
//! ## 工具清单（设计 §6）
//!
//! | 工具 | 用途 | 鉴权 |
//! |------|------|------|
//! | `doc_list` | 目录树逐级浏览（path 缺省根；query/offset/limit 过滤分页） | 只读 |
//! | `doc_read` | 读取文档（doc_id 或 path） | 只读 |
//! | `doc_pull` | 下载本地缓存副本 + base_version（供编辑后提交） | 只读 |
//! | `doc_add` | **add to doc**：新增文档直接生效（快照导入，含 import 来源） | 写 |
//! | `doc_submit_update` | PR 式更新请求（不直接写库） | 写 |
//! | `doc_check_request` | 查询更新请求审核状态 | 只读 |
//! | `doc_mkdir` | 创建子目录 | 写 |
//! | `doc_search` | 跨目录关键字检索 | 只读 |
//!
//! ## 寻址约定
//!
//! 文档参数接受 **doc_id**（`doc-{hex}`）或 **path**（`项目A/纪要.md`，UTF-8
//! 相对路径，缺省根）；目录参数接受 path（`项目A/子组`，空串 = 根）。服务端
//! 把 path 解析为内部 `dir_id` + 文件名（库内同级唯一，见
//! `mcp::tools::resolve`）。

/// MCP 工具 manifest（编译时常量，便于 LLM 直接 prompt-include）。
pub const DOC_TOOL_MANIFEST: &str = r#"{
  "schema_version": "2025-06-18",
  "tools": [
    {
      "name": "doc_list",
      "description": "List documents and sub-directories under a directory path (root when omitted). Returns id, name, kind and version for each entry.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "path": {
            "type": "string",
            "description": "Directory path relative to the library root, e.g. \"研发/文档库\". Empty = root.",
            "default": ""
          },
          "query": {
            "type": "string",
            "description": "Optional case-insensitive substring filter on entry names.",
            "default": ""
          },
          "offset": { "type": "integer", "minimum": 0, "default": 0 },
          "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 100 }
        }
      }
    },
    {
      "name": "doc_read",
      "description": "Read a shared document's full Markdown content with its version and metadata. Accepts a doc_id or a path like \"研发/纪要.md\".",
      "inputSchema": {
        "type": "object",
        "required": ["ref"],
        "properties": {
          "ref": {
            "type": "string",
            "description": "doc_id (doc-xxxx) or relative path ending in .md"
          }
        }
      }
    },
    {
      "name": "doc_pull",
      "description": "Fetch a document for editing: returns content, base_version and the suggested cache path. Edit the content, then submit via doc_submit_update with the same base_version.",
      "inputSchema": {
        "type": "object",
        "required": ["ref"],
        "properties": {
          "ref": {
            "type": "string",
            "description": "doc_id (doc-xxxx) or relative path ending in .md"
          }
        }
      }
    },
    {
      "name": "doc_add",
      "description": "Add a NEW document (add-to-doc snapshot import). Takes effect immediately without review. Fails with a name conflict when a document with the same title already exists in the target directory.",
      "inputSchema": {
        "type": "object",
        "required": ["content"],
        "properties": {
          "path": {
            "type": "string",
            "description": "Target directory path (root when empty).",
            "default": ""
          },
          "title": {
            "type": "string",
            "description": "Document title (filename stem, .md appended). Defaults to the source_path stem when omitted."
          },
          "content": {
            "type": "string",
            "description": "Full Markdown content of the new document."
          },
          "source_workspace": {
            "type": "string",
            "description": "Agent workspace id/name the snapshot came from (recorded in import metadata)."
          },
          "source_path": {
            "type": "string",
            "description": "Original file path the snapshot came from (recorded in import metadata; used as title fallback)."
          }
        }
      }
    },
    {
      "name": "doc_submit_update",
      "description": "Submit a PR-style update request for a document. Does NOT write the library directly: a human must approve it. Fails with a version conflict when base_version is stale — pull again and rebase.",
      "inputSchema": {
        "type": "object",
        "required": ["ref", "content", "base_version"],
        "properties": {
          "ref": {
            "type": "string",
            "description": "doc_id (doc-xxxx) or relative path ending in .md"
          },
          "content": {
            "type": "string",
            "description": "New full Markdown content to propose."
          },
          "base_version": {
            "type": "integer",
            "minimum": 1,
            "description": "Version the edit is based on (from doc_read / doc_pull)."
          }
        }
      }
    },
    {
      "name": "doc_check_request",
      "description": "Poll the review status of an update request: pending | approved | rejected | expired. Returns the review note when reviewed.",
      "inputSchema": {
        "type": "object",
        "required": ["request_id"],
        "properties": {
          "request_id": {
            "type": "string",
            "pattern": "^r-[a-f0-9]{12}$",
            "description": "Request id returned by doc_submit_update."
          }
        }
      }
    },
    {
      "name": "doc_mkdir",
      "description": "Create a sub-directory. The path's parent must already exist; create levels one at a time.",
      "inputSchema": {
        "type": "object",
        "required": ["path"],
        "properties": {
          "path": {
            "type": "string",
            "description": "Full relative path of the new directory, e.g. \"研发/文档库\"."
          }
        }
      }
    },
    {
      "name": "doc_search",
      "description": "Cross-directory keyword search over document titles and bodies. Title matches rank above content matches.",
      "inputSchema": {
        "type": "object",
        "required": ["keyword"],
        "properties": {
          "keyword": { "type": "string", "minLength": 1 },
          "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
        }
      }
    }
  ]
}"#;

/// 解析 manifest 中的工具数组（`tools/list` 响应体）。
pub fn manifest_tools() -> Vec<serde_json::Value> {
    let v: serde_json::Value =
        serde_json::from_str(DOC_TOOL_MANIFEST).expect("DOC_TOOL_MANIFEST must be valid JSON");
    v["tools"].as_array().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_to_expected_tool_count() {
        let tools = manifest_tools();
        assert_eq!(tools.len(), 8, "design §6 lists 8 doc_* tools");
        for t in &tools {
            assert!(t["name"].as_str().unwrap().starts_with("doc_"));
            assert!(t["description"].as_str().unwrap().len() > 10);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }
}

