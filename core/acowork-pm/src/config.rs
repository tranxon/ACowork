//! PM 服务配置加载。
//!
//! 配置来源（按优先级降序）：
//!
//! 1. 环境变量（`ACOWORK_PM_*`）
//! 2. TOML 配置文件（通过 `--config <path>` 传入，默认 `./acowork-pm.toml`）
//! 3. [`PmConfig::default`]
//!
//! Gateway 把 `PmConfig` 嵌入到自身配置树（[`acowork-gateway::config::GatewayConfig::pm`]），
//! 启动时构造本服务实例。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// PM 服务运行时配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PmConfig {
    /// 数据根目录（项目/任务/附件存储位置）。
    ///
    /// 解析顺序：环境变量 `ACOWORK_PM_DATA_DIR` → TOML `data_dir` → [`directories::ProjectDirs`]。
    /// 嵌入 Gateway 时由 Gateway 覆写为 `{gateway.data_dir}/acowork-pm`（见
    /// [`acowork-gateway::config::GatewayConfig::prepare_pm_data_dir`]）。
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// HTTP 监听地址（仅供 Gateway 内嵌模式使用，独立进程模式由 `--bind` 覆盖）。
    ///
    /// 默认 `127.0.0.1:0`（由 Gateway 分配端口）。
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,

    /// 任务最大嵌套深度（防滥用 + UI 折叠层数合理）。
    ///
    /// 默认 `5`（根任务 + 4 层子任务）。
    #[serde(default = "default_max_task_depth")]
    pub max_task_depth: u8,

    /// 单个附件文件最大字节数。
    ///
    /// 默认 `10 MiB`。超过返回 400 `attachment_too_large`。
    #[serde(default = "default_max_attachment_size")]
    pub max_attachment_size: u64,

    /// 单个任务下附件数量上限。
    ///
    /// 默认 `20`。超过返回 400 `too_many_attachments`。
    #[serde(default = "default_max_attachments_per_task")]
    pub max_attachments_per_task: usize,

    /// 已删除项目/任务在 `.trash/` 中保留天数。
    ///
    /// 默认 `30`。过期后下次启动时清理。
    #[serde(default = "default_trash_retention_days")]
    pub trash_retention_days: u32,

    /// 启动时强制 walkdir 重建索引（开发期 true，生产期 false 走增量）。
    #[serde(default = "default_index_rebuild_on_start")]
    pub index_rebuild_on_start: bool,

    /// 启用图片附件缩略图生成（需 `image-thumb` feature）。
    ///
    /// 默认 `true`。关闭可加速上传但前端预览需直接拉原图。
    #[serde(default = "default_true")]
    pub generate_thumbnails: bool,

    /// 缩略图最大边长（像素）。
    ///
    /// 默认 `256`。
    #[serde(default = "default_thumbnail_max_edge")]
    pub thumbnail_max_edge: u32,

    /// 是否自动把 pm MCP HTTP 端点注入每个 Agent 的 MCP catalog（设计 §6.1 / T3-4）。
    ///
    /// 默认 `true`：Gateway 在 `acowork/global/mcps` 资源下发中附带一个
    /// `name = "pm"`、transport = http 的 MCP server，Agent 启动后自动获得
    /// `pm_*` 工具。关闭后 Agent 需在 Tools 面板手动添加（通常无需关闭）。
    #[serde(default = "default_true")]
    pub auto_inject_mcp: bool,

    /// pm MCP HTTP 端点的公开路径（含 `/api/pm` 前缀，由 Gateway `nest_service`
    /// 挂载）。默认 `/api/pm/mcp`（设计 §21）。
    #[serde(default = "default_mcp_http_path")]
    pub mcp_http_path: String,
}

fn default_true() -> bool {
    true
}

fn default_mcp_http_path() -> String {
    "/api/pm/mcp".to_string()
}

fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("com", "acowork", "pm")
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./data/acowork-pm"))
}

fn default_bind_addr() -> String {
    "127.0.0.1:0".to_string()
}

fn default_max_task_depth() -> u8 {
    5
}

fn default_max_attachment_size() -> u64 {
    10 * 1024 * 1024 // 10 MiB
}

fn default_max_attachments_per_task() -> usize {
    20
}

fn default_trash_retention_days() -> u32 {
    30
}

fn default_index_rebuild_on_start() -> bool {
    true
}

fn default_thumbnail_max_edge() -> u32 {
    256
}

impl Default for PmConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            bind_addr: default_bind_addr(),
            max_task_depth: default_max_task_depth(),
            max_attachment_size: default_max_attachment_size(),
            max_attachments_per_task: default_max_attachments_per_task(),
            trash_retention_days: default_trash_retention_days(),
            index_rebuild_on_start: default_index_rebuild_on_start(),
            generate_thumbnails: default_true(),
            thumbnail_max_edge: default_thumbnail_max_edge(),
            auto_inject_mcp: default_true(),
            mcp_http_path: default_mcp_http_path(),
        }
    }
}

impl PmConfig {
    /// 校验配置合法性（如路径可创建、深度为正、限额为正）。
    pub fn validate(&self) -> crate::Result<()> {
        use crate::error::PmError;

        if self.max_task_depth == 0 {
            return Err(PmError::Internal(
                "max_task_depth must be > 0".to_string(),
            ));
        }
        if self.max_attachment_size == 0 {
            return Err(PmError::Internal(
                "max_attachment_size must be > 0".to_string(),
            ));
        }
        if self.max_attachments_per_task == 0 {
            return Err(PmError::Internal(
                "max_attachments_per_task must be > 0".to_string(),
            ));
        }
        Ok(())
    }

    /// 项目根目录 = `{data_dir}/projects`。
    pub fn projects_dir(&self) -> PathBuf {
        self.data_dir.join("projects")
    }

    /// Trash 根目录 = `{data_dir}/.trash`。
    pub fn trash_dir(&self) -> PathBuf {
        self.data_dir.join(".trash")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_validates() {
        PmConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_zero_depth() {
        let mut cfg = PmConfig::default();
        cfg.max_task_depth = 0;
        assert!(cfg.validate().is_err());
    }
}