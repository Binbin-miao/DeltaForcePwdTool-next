//! 角色模板库加载（spec D3：**N = 该角色目录下连续编号图片的最大编号**）。
//!
//! 上游 `images/<角色>/<1..N>.png` 实测：埃德温 4 / 克莱尔 6 / 卢卡斯 6 / 巴斯特 6 /
//! 德穆兰 6 / 无名 6 / 罗米修斯 6 / 雅各布 6 / 格赫罗斯 8，共 54 张。
//! 目录里的「文件个数」不等于 N —— 编号出现空洞（1,3,5）时必须取连续前缀的长度，
//! 否则投票得分归一化的分母会偏小，模板张数少的角色被虚高。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use delta_vision::GrayImage;

use crate::TemplateError;

/// 单张模板：编号（即上游的模板序号，点击序以此升序为准，spec §2.2 不变量 3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub no: u32,
    pub image: GrayImage,
}

/// 一个角色：`n` 恒等于 `templates.len()`，且模板编号恰为 `1..=n`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub templates: Vec<Template>,
}

impl Role {
    /// spec D3：模式 N 直接由模板张数给出，不再读数字 OCR。
    pub fn n(&self) -> u32 {
        self.templates.len() as u32
    }
}

/// 角色模板库。加载期间的退化情况（单张读失败、编号空洞）全部记入 `warnings`，
/// 不静默吞掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleLibrary {
    roles: Vec<Role>,
    warnings: Vec<String>,
}

impl RoleLibrary {
    /// 扫描 `root/<角色>/<1..N>.png`。
    ///
    /// 退化契约：
    /// - 单张图片解码失败 → 跳过并记 warn（该编号从连续前缀中消失，N 相应收缩）；
    /// - 编号存在空洞 → 空洞之后的模板丢弃并记 warn（保证 `templates == 1..=n`）；
    /// - 某角色一张都读不出来 → 跳过该角色并记 warn；
    /// - 全部角色都为空 → [`TemplateError::NoRoles`]。
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self, TemplateError> {
        let root = root.as_ref();
        let mut entries = read_dir_sorted(root)?;
        entries.retain(|path| path.is_dir());
        if entries.is_empty() {
            return Err(TemplateError::NoRoles { root: root.to_path_buf() });
        }

        let mut roles = Vec::new();
        let mut warnings = Vec::new();
        for dir in entries {
            match load_role(&dir, &mut warnings) {
                Ok(role) => roles.push(role),
                Err(reason) => warnings.push(format!("跳过角色 {}：{reason}", dir.display())),
            }
        }
        roles.sort_by(|a, b| a.name.cmp(&b.name));
        Self::new(roles, warnings)
    }

    /// 直接用已解码的角色构造（供上层做「一键补录模板」时替换库）。
    pub fn from_roles(roles: Vec<Role>) -> Result<Self, TemplateError> {
        Self::new(roles, Vec::new())
    }

    fn new(roles: Vec<Role>, warnings: Vec<String>) -> Result<Self, TemplateError> {
        if roles.is_empty() {
            return Err(TemplateError::NoRoles {
                root: PathBuf::new(),
            });
        }
        Ok(Self { roles, warnings })
    }

    pub fn roles(&self) -> &[Role] {
        &self.roles
    }

    /// 加载期的全部告警，UI 需原样呈现（失败必留证，spec D10）。
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn find(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|role| role.name == name)
    }
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, TemplateError> {
    let reader = std::fs::read_dir(dir).map_err(|source| TemplateError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mut paths = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|source| TemplateError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

/// 载入单个角色目录；`templates` 保证编号连续从 1 开始。
fn load_role(dir: &Path, warnings: &mut Vec<String>) -> Result<Role, String> {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut numbered = BTreeMap::new();
    for path in read_dir_sorted(dir).map_err(|error| error.to_string())? {
        // 只认纯数字文件名的图片；`x.png` 之类的补录命名不参与 N 的推导。
        if !is_png(&path) {
            continue;
        }
        let Some(no) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u32>().ok())
        else {
            continue;
        };
        if no == 0 {
            warnings.push(format!("{}：模板编号 0 非法，已忽略", path.display()));
            continue;
        }
        match decode_png_gray(&path) {
            Ok(image) => {
                numbered.insert(no, image);
            }
            Err(error) => warnings.push(format!("{}：{error}", path.display())),
        }
    }

    // spec D3：N = 连续编号的最大编号（空洞之后的编号不计入 N）。
    let mut contiguous = Vec::new();
    let mut expected = 1u32;
    while let Some(image) = numbered.remove(&expected) {
        contiguous.push(Template {
            no: expected,
            image,
        });
        expected += 1;
    }
    if let Some(hole) = contiguous.len().checked_add(1).filter(|_| !numbered.is_empty()) {
        let dropped: Vec<String> = numbered.keys().map(u32::to_string).collect();
        warnings.push(format!(
            "{name}：编号 {hole} 缺失，编号 [{}] 的模板已丢弃（N 保持为连续前缀长度）",
            dropped.join(",")
        ));
    }
    if contiguous.is_empty() {
        return Err("没有任何可用的连续编号模板".to_string());
    }
    Ok(Role {
        name,
        templates: contiguous,
    })
}

fn is_png(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
}

/// PNG → 灰度。BT.601 整数近似，与 `GrayImage::from_bgra` 保持同一亮度公式。
pub fn decode_png_gray(path: &Path) -> Result<GrayImage, TemplateError> {
    let file = File::open(path).map_err(|source| TemplateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().map_err(|source| TemplateError::Png {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|source| TemplateError::Png {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    let bytes = &buffer[..info.buffer_size()];
    let (w, h) = (info.width, info.height);

    let palette = reader.info().palette.clone();
    let luma = match info.color_type {
        png::ColorType::Grayscale => bytes.to_vec(),
        png::ColorType::GrayscaleAlpha => bytes.as_chunks::<2>().0.iter().map(|pair| pair[0]).collect(),
        png::ColorType::Rgb => bytes
            .as_chunks::<3>()
            .0
            .iter()
            .map(|rgb| luma(rgb[0], rgb[1], rgb[2]))
            .collect(),
        png::ColorType::Rgba => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|rgba| luma(rgba[0], rgba[1], rgba[2]))
            .collect(),
        png::ColorType::Indexed => {
            let palette = palette.ok_or_else(|| TemplateError::Png {
                path: path.to_path_buf(),
                source: "索引色图片缺少调色板".into(),
            })?;
            bytes
                .iter()
                .map(|&index| {
                    let base = usize::from(index) * 3;
                    palette
                        .get(base..base + 3)
                        .map(|rgb| luma(rgb[0], rgb[1], rgb[2]))
                        .unwrap_or(0)
                })
                .collect()
        }
    };
    GrayImage::new(w, h, luma).map_err(TemplateError::Vision)
}

fn luma(r: u8, g: u8, b: u8) -> u8 {
    let weighted = 77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b);
    (weighted >> 8) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("delta-tpl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 用 t4 自带的 `images/` 真实素材验证 N 的推导规则。
    #[test]
    fn library_derives_n_from_real_assets() {
        let lib = RoleLibrary::load_from_dir(crate::IMAGES_DIR).expect("真实素材必须可加载");
        assert_eq!(lib.roles().len(), 9, "上游 9 个角色");
        let total: u32 = lib.roles().iter().map(Role::n).sum();
        assert_eq!(total, 54, "上游 54 张模板");

        let expected = [
            ("埃德温", 4u32),
            ("克莱尔", 6),
            ("卢卡斯", 6),
            ("巴斯特", 6),
            ("德穆兰", 6),
            ("无名", 6),
            ("罗米修斯", 6),
            ("雅各布", 6),
            ("格赫罗斯", 8),
        ];
        for (name, n) in expected {
            let role = lib.find(name).unwrap_or_else(|| panic!("缺少角色 {name}"));
            assert_eq!(role.n(), n, "{name} 的 N 必须等于模板张数");
            let numbers: Vec<u32> = role.templates.iter().map(|t| t.no).collect();
            assert_eq!(numbers, (1..=n).collect::<Vec<u32>>(), "{name} 编号必须连续");
        }
        assert!(lib.warnings().is_empty(), "真实素材不应产生告警：{:?}", lib.warnings());
    }

    /// 编号有洞时 N 取连续前缀长度，而不是文件个数。
    #[test]
    fn library_uses_contiguous_prefix_when_numbers_have_holes() {
        let root = scratch_dir("hole");
        let role_dir = root.join("测试角色");
        std::fs::create_dir_all(&role_dir).expect("建角色目录");
        for no in [1u32, 2, 4, 5] {
            let image = GrayImage::filled(4, 4, (no * 10) as u8).expect("合法图");
            write_png(&role_dir.join(format!("{no}.png")), &image);
        }
        let lib = RoleLibrary::load_from_dir(&root).expect("加载成功");
        let role = lib.find("测试角色").expect("角色存在");
        assert_eq!(role.n(), 2, "连续前缀 1,2 之后的 4,5 必须丢弃");
        assert_eq!(role.templates[1].no, 2);
        let warnings = lib.warnings().join("|");
        assert!(warnings.contains("编号 3 缺失"), "{warnings}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn library_skips_broken_template_but_keeps_others() {
        let root = scratch_dir("broken");
        let role_dir = root.join("角色B");
        std::fs::create_dir_all(&role_dir).expect("建角色目录");
        let image = GrayImage::filled(4, 4, 9).expect("合法图");
        write_png(&role_dir.join("1.png"), &image);
        std::fs::write(role_dir.join("2.png"), b"not a png").expect("写入坏文件");
        write_png(&role_dir.join("3.png"), &image);

        let lib = RoleLibrary::load_from_dir(&root).expect("加载成功");
        let role = lib.find("角色B").expect("角色存在");
        assert_eq!(role.n(), 1, "坏图导致编号 2 缺失，N 收缩到 1");
        assert!(!lib.warnings().is_empty(), "坏图必须留证");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_root_is_an_error_not_an_empty_library() {
        let root = scratch_dir("empty");
        let error = RoleLibrary::load_from_dir(&root).expect_err("空目录必须报错");
        assert!(matches!(error, TemplateError::NoRoles { .. }), "{error:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn write_png(path: &Path, image: &GrayImage) {
        let file = File::create(path).expect("建文件");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.w, image.h);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("写头");
        writer.write_image_data(image.as_slice()).expect("写数据");
    }
}
