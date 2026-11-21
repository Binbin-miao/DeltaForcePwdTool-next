//! 配置持久化：原子写 + 只读像素视图（t1 任务 4）。
//!
//! 写盘顺序固定为 `临时文件 → sync_all → rename`；`rename` 是唯一的提交点，
//! 因此崩溃或写失败都不会破坏已有的配置文件。
//! 加载时缺失文件视为首次运行（用默认值），其它 I/O 错误一律上报，不静默吞掉。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{Config, ConfigError, PxView};

/// 配置文件默认文件名（放在 exe 同目录）。
pub const CONFIG_FILE_NAME: &str = "delta-pwd-tool.config.json";

/// 一次加载的结果。
///
/// `view == None` **严格等价于**「主显示器尚未校准」；此时 `config.validate()`
/// 必定包含 [`crate::MissingArea::Monitor`]，UI 据此灰置全部流程入口（spec D9）。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub view: Option<PxView>,
}

impl Loaded {
    /// 取像素视图；未校准或换算越界时返回明确错误。
    pub fn view(&self) -> Result<&PxView, ConfigError> {
        self.view.as_ref().ok_or(ConfigError::Uncalibrated)
    }
}

/// 读取配置。文件不存在 → 默认配置；字段缺失 → 该字段默认值；未知字段 → 忽略。
pub fn load(path: &Path) -> Result<Loaded, ConfigError> {
    let config = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| ConfigError::Parse {
            path: display(path),
            source,
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(source) => {
            return Err(ConfigError::Io {
                action: "读取配置文件",
                path: display(path),
                source,
            })
        }
    };
    // 开箱即用：从未校准过的区域用内置坐标补齐（用户已校准的值一律保留）。
    // 这样新装、旧空配置、换机器都能直接跑，不必先进游戏框选。
    let config = config.with_builtin_regions();
    // 关键不变量：像素值每次都从百分比重新换算，绝不回填进 config。
    let view = if config.monitor.is_calibrated() {
        Some(config.try_px_view()?)
    } else {
        None
    };
    Ok(Loaded { config, view })
}

/// 原子保存配置。只序列化百分比字段，`save` 的入参里不存在任何像素类型。
pub fn save(path: &Path, config: &Config) -> Result<(), ConfigError> {
    let bytes = serde_json::to_vec_pretty(config).map_err(ConfigError::Serialize)?;
    write_atomic(path, &bytes)
}

/// `临时文件 → fsync → rename`；任何一步失败都保留原文件不动。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let temp = temp_path(path);
    let outcome = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(source) = outcome {
        let _ = fs::remove_file(&temp);
        return Err(ConfigError::Io {
            action: "写入临时配置文件",
            path: display(&temp),
            source,
        });
    }
    fs::rename(&temp, path).map_err(|source| ConfigError::Io {
        action: "原子替换配置文件",
        path: display(path),
        source,
    })
}

/// 临时文件与目标同目录，保证 `rename` 不跨卷。
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MissingArea, RegionEntry, CANDIDATE_BOX_COUNT, CONFIG_FILE_NAME, MORSE_REGION_COUNT,
    };
    use delta_geom::{MonitorInfo, PctPoint, PctRect};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "delta-config-test-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("创建测试临时目录");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_config() -> Config {
        let mut config = Config {
            monitor: MonitorInfo::new(2560, 1440).expect("固定分辨率"),
            regions: (0..MORSE_REGION_COUNT)
                .map(|index| RegionEntry {
                    name: format!("morse_cell_{}", index + 1),
                    rect: PctRect {
                        x: 0.125,
                        y: 0.2 + 0.1 * index as f32,
                        w: 0.25,
                        h: 0.0625,
                    },
                })
                .collect(),
            confirm_clicks: vec![PctPoint { x: 0.5, y: 0.875 }],
            ..Config::default()
        };
        config.console.col_x = vec![0.25, 0.375, 0.5, 0.625];
        config.console.row_y = vec![0.25, 0.375, 0.5, 0.625];
        config
    }

    #[test]
    fn hundred_loads_do_not_drift() {
        let dir = TempDir::new();
        let path = dir.join(CONFIG_FILE_NAME);
        save(&path, &sample_config()).expect("首次保存");
        let bytes_before = fs::read(&path).expect("读取落盘内容");
        let first = load(&path).expect("加载").view.expect("已校准应有像素视图");

        for round in 0..100 {
            let loaded = load(&path).expect("加载");
            assert_eq!(
                loaded.view.expect("已校准应有像素视图"),
                first,
                "第 {round} 次 load 后像素视图漂移（上游二次缩放缺陷复现）"
            );
        }
        assert_eq!(
            fs::read(&path).expect("读取落盘内容"),
            bytes_before,
            "load 不得改动磁盘上的百分比"
        );

        // 连续 load 后再次 save：必须**收敛**，证明像素值没有被回写。
        // 首次 save 会把空区域补齐成内置坐标（这是预期行为），之后必须字节级稳定。
        let reloaded = load(&path).expect("加载");
        save(&path, &reloaded.config).expect("二次保存");
        let after_fill = fs::read(&path).expect("读取落盘内容");
        for round in 0..3 {
            let again = load(&path).expect("加载");
            save(&path, &again.config).expect("再次保存");
            assert_eq!(
                fs::read(&path).expect("读取落盘内容"),
                after_fill,
                "第 {round} 次 load→save 后字节漂移（像素值被回写或内置值重复合并）"
            );
        }
    }

    #[test]
    fn save_serializes_percentages_only() {
        let dir = TempDir::new();
        let path = dir.join(CONFIG_FILE_NAME);
        save(&path, &sample_config()).expect("保存");
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("读取")).expect("解析");
        let region = &json["regions"][0]["rect"];
        assert_eq!(region["x"].as_f64(), Some(0.125));
        assert_eq!(region["w"].as_f64(), Some(0.25));
        assert_eq!(json["console"]["col_x"][3].as_f64(), Some(0.625));
        assert_eq!(json["monitor"]["w"].as_i64(), Some(2560));
        // 像素量级（如 320/2048）绝不应出现在区域字段里。
        assert!(region["x"].as_f64().is_some_and(|v| v <= 1.0));
    }

    #[test]
    fn missing_file_and_missing_fields_fall_back_to_defaults() {
        let dir = TempDir::new();
        let absent = dir.join("absent.json");
        let loaded = load(&absent).expect("文件缺失视为首次运行");
        // 首次运行即填内置坐标：不校准也能直接用（这是「开箱即用」的核心保证）。
        assert_eq!(loaded.config, Config::default().with_builtin_regions());
        assert!(loaded.config.regions.iter().all(|entry| entry.rect.w > 0.0));
        assert!(loaded.view.is_none());
        assert_eq!(loaded.view().unwrap_err().to_string(), "主显示器尺寸未校准，无法生成像素视图：需先运行校准向导");

        let partial = dir.join("partial.json");
        fs::write(&partial, br#"{"ocr_enabled": false, "future_field": { "a": 1 }}"#)
            .expect("写入部分配置");
        let loaded = load(&partial).expect("部分配置");
        assert!(!loaded.config.ocr_enabled);
        assert_eq!(loaded.config.timings.morse_after_input_ms, 600);
        assert_eq!(loaded.config.regions.len(), MORSE_REGION_COUNT);
        // 部分配置同样会被内置坐标补齐：不再报「指纹格缺失」，
        // 只剩「主显示器未校准」一项（那是换算像素时才需要的锚点）。
        assert_eq!(
            loaded.config.fingerprint.candidate_boxes.len(),
            CANDIDATE_BOX_COUNT
        );
        assert!(loaded.config.fingerprint.candidate_boxes[0].w > 0.0);
        assert_eq!(loaded.config.validate(), vec![MissingArea::Monitor]);
    }

    #[test]
    fn atomic_write_failure_keeps_original_file() {
        let dir = TempDir::new();
        let path = dir.join(CONFIG_FILE_NAME);
        save(&path, &sample_config()).expect("首次保存");
        let original = fs::read(&path).expect("读取原始内容");

        // 用同名目录占住临时文件路径，令 File::create 失败。
        let temp = temp_path(&path);
        fs::create_dir(&temp).expect("创建临时路径占位目录");

        let mut changed = sample_config();
        changed.always_on_top = true;
        let error = save(&path, &changed).expect_err("写临时文件必须失败");
        assert!(error.to_string().contains("写入临时配置文件"), "{error}");
        assert_eq!(fs::read(&path).expect("读取原始内容"), original);

        fs::remove_dir(&temp).expect("清理占位目录");
        save(&path, &changed).expect("恢复后可正常保存");
        assert!(load(&path).expect("加载").config.always_on_top);
    }

    #[test]
    fn legacy_accepted_disclaimer_field_is_silently_ignored() {
        // 免责声明已从产品中移除，但老用户的配置里还留着这个键。
        // 它必须被静默忽略：不能报错、不能因未知字段让整份配置回退成默认值。
        let dir = TempDir::new();
        let path = dir.join(CONFIG_FILE_NAME);
        let mut json = serde_json::to_value(sample_config()).expect("序列化");
        json["accepted_disclaimer"] = serde_json::Value::Bool(true);
        // 同时保留一个可观测的非默认值，证明整份配置真的被读进来了。
        json["always_on_top"] = serde_json::Value::Bool(true);
        fs::write(&path, serde_json::to_vec(&json).expect("编码")).expect("写入老配置");

        let loaded = load(&path).expect("带 legacy 键的老配置必须能加载");
        assert!(loaded.config.always_on_top, "其余字段照常生效");
        assert!(
            loaded.config.validate().is_empty(),
            "老配置不该被降级成默认值：{:?}",
            loaded.config.validate()
        );
    }

    #[test]
    fn confirm_clicks_roundtrip_preserves_empty_array() {
        let dir = TempDir::new();
        let path = dir.join(CONFIG_FILE_NAME);
        let mut config = sample_config();
        config.confirm_clicks.clear();
        save(&path, &config).expect("保存");
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("读取")).expect("解析");
        assert_eq!(json["confirm_clicks"].as_array().map(Vec::len), Some(0));
        assert!(load(&path).expect("加载").config.confirm_clicks.is_empty());
    }

    #[test]
    fn corrupt_json_reports_parse_error_with_path() {
        let dir = TempDir::new();
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, b"{ not json").expect("写入损坏配置");
        let error = load(&path).expect_err("损坏 JSON 必须报错");
        assert!(error.to_string().contains("配置 JSON 解析失败"), "{error}");
        assert!(error.to_string().contains(CONFIG_FILE_NAME), "{error}");
    }
}
