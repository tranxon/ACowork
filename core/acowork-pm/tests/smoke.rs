//! P0 阶段烟雾测试。
//!
//! 验证：
//! - crate 可被外部使用
//! - 关键 re-export 存在
//! - PmConfig 默认值合法
//! - TreePmStore 可构造（无需重建索引）

use acowork_pm::{PmConfig, ProjectId, TaskId, AttachmentId};

#[test]
fn crate_re_exports_compile() {
    // 编译期验证：这些类型必须从 lib.rs 顶层导出
    let _: ProjectId = ProjectId::generate();
    let _: TaskId = TaskId::generate();
    let _: AttachmentId = AttachmentId::generate();
}

#[test]
fn default_config_is_valid() {
    let cfg = PmConfig::default();
    cfg.validate().expect("default config must validate");
}

#[test]
fn config_projects_dir_is_under_data_dir() {
    let cfg = PmConfig::default();
    assert!(cfg.projects_dir().starts_with(&cfg.data_dir));
    assert!(cfg.trash_dir().starts_with(&cfg.data_dir));
}

#[tokio::test]
async fn store_constructs_with_tempdir() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = PmConfig::default();
    cfg.data_dir = tmp.path().to_path_buf();

    let store = acowork_pm::TreePmStore::new(cfg)
        .await
        .expect("store construction should succeed");
    assert_eq!(store.indexed_task_count(), 0);
}

#[test]
fn id_roundtrip() {
    let pid = ProjectId::generate();
    let s = pid.to_string();
    let parsed: ProjectId = s.parse().expect("parse must succeed");
    assert_eq!(pid, parsed);
}