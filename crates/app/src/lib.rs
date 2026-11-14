//! `delta-app`：装配层（单实例 → 配置 → 字体 → 界面 → 热键 → 后台识别）。

pub mod app;
pub mod keyhook;
pub mod platform;
pub mod worker;

pub use app::DeltaApp;

use std::path::PathBuf;

/// 可执行文件所在目录（打包后所有资源都相对它）。
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 配置文件路径：`<exe 目录>/settings.json`（原子写由 `delta-config` 负责）。
pub fn config_path() -> PathBuf {
    exe_dir().join(delta_config::CONFIG_FILE_NAME)
}

/// 角色模板目录：优先 exe 旁边的 `images/`（便于用户补录），否则退回编译期路径。
pub fn images_dir() -> PathBuf {
    let beside_exe = exe_dir().join("images");
    if beside_exe.is_dir() {
        beside_exe
    } else {
        PathBuf::from(delta_templates::IMAGES_DIR)
    }
}

/// 任务留证目录（`exe 同级 / logs`）。
///
/// **必须是 exe 同级**，不能用相对路径 `logs`：双击启动时进程 cwd 可能是
/// `C://Windows//System32`，写进去的文件用户永远找不到，等于没留证。
pub fn logs_dir() -> PathBuf {
    let dir = exe_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 失败留证目录（spec D10：失败必留证）。
pub fn evidence_dir() -> PathBuf {
    let dir = exe_dir().join("evidence");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_sits_beside_the_executable() {
        let path = config_path();
        assert!(path.ends_with(delta_config::CONFIG_FILE_NAME));
        assert_eq!(path.parent(), Some(exe_dir().as_path()));
    }

    #[test]
    fn images_dir_always_points_somewhere_existing_or_builtin() {
        // 开发/打包两种布局都必须给出一个非空路径（打包后由用户补录目录覆盖）。
        assert!(!images_dir().as_os_str().is_empty());
    }
}
