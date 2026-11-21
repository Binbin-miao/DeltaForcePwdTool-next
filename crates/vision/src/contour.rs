//! 外轮廓提取：8 连通 + 包围盒。
//!
//! 语义与 OpenCV `RETR_EXTERNAL + CHAIN_APPROX_SIMPLE` 对齐：
//! 只返回**外轮廓**（连通域级），不返回洞，也不返回层级；
//! `CHAIN_APPROX_SIMPLE` 只压缩共线点，包围盒完全一致。
//!
//! 实现用并查集（全迭代、路径压缩），**禁止递归 flood fill**（大区域会栈溢出，t3 任务 3）。

use crate::{BinaryImage, Region};

/// 一个外轮廓：包围盒 + 面积（前景像素数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contour {
    pub region: Region,
    pub area: u32,
}

impl Contour {
    /// 宽高比（spec §2.2 不变量 7 的点划判据输入）。
    pub fn aspect(&self) -> f32 {
        self.region.aspect()
    }
}

/// 提取全部外轮廓，按包围盒左上角 `(y, x)` 行主序返回，顺序稳定。
pub fn external_contours(binary: &BinaryImage) -> Vec<Contour> {
    let w = binary.width() as usize;
    let h = binary.height() as usize;
    if w == 0 || h == 0 {
        return Vec::new();
    }

    // 标签图：0 = 无标签，其它值为并查集节点编号。
    let mut labels = vec![0u32; w * h];
    let mut parent: Vec<u32> = vec![0];

    // 第一遍：单遍扫描 + 8 连通合并（只看已扫描的 左/左上/上/右上）。
    for y in 0..h {
        for x in 0..w {
            if !binary.get(x as u32, y as u32) {
                continue;
            }
            let mut smallest = 0u32;
            // 只看已扫描过的四个 8 连通邻居：左、左上、上、右上。
            let above = y.checked_sub(1);
            let neighbors = [
                x.checked_sub(1).map(|nx| (nx, y)),
                x.checked_sub(1).zip(above),
                above.map(|ny| (x, ny)),
                above.and_then(|ny| (x + 1 < w).then_some((x + 1, ny))),
            ];
            for (nx, ny) in neighbors.into_iter().flatten() {
                let label = labels[ny * w + nx];
                if label == 0 {
                    continue;
                }
                if smallest == 0 {
                    smallest = label;
                } else {
                    union(&mut parent, smallest, label);
                }
            }
            if smallest == 0 {
                smallest = parent.len() as u32;
                parent.push(smallest);
            }
            labels[y * w + x] = smallest;
        }
    }

    // 第二遍：按根节点累计包围盒与面积。
    let mut stats = vec![ContourAccumulator::default(); parent.len()];
    for y in 0..h {
        for x in 0..w {
            let label = labels[y * w + x];
            if label == 0 {
                continue;
            }
            let root = find(&mut parent, label) as usize;
            stats[root].push(x as u32, y as u32);
        }
    }

    let mut contours: Vec<Contour> = stats
        .into_iter()
        .filter(|accumulator| accumulator.area > 0)
        .map(ContourAccumulator::into_contour)
        .collect();
    contours.sort_by_key(|contour| (contour.region.y, contour.region.x));
    contours
}

/// 迭代查找 + 路径压缩。
fn find(parent: &mut [u32], label: u32) -> u32 {
    let mut root = label;
    while parent[root as usize] != root {
        root = parent[root as usize];
    }
    let mut current = label;
    while parent[current as usize] != root {
        let next = parent[current as usize];
        parent[current as usize] = root;
        current = next;
    }
    root
}

/// 小者当根。
fn union(parent: &mut [u32], a: u32, b: u32) {
    let root_a = find(parent, a);
    let root_b = find(parent, b);
    if root_a == root_b {
        return;
    }
    if root_a < root_b {
        parent[root_b as usize] = root_a;
    } else {
        parent[root_a as usize] = root_b;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ContourAccumulator {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
    area: u32,
}

impl ContourAccumulator {
    fn push(&mut self, x: u32, y: u32) {
        if self.area == 0 {
            self.min_x = x;
            self.min_y = y;
            self.max_x = x;
            self.max_y = y;
        } else {
            self.min_x = self.min_x.min(x);
            self.min_y = self.min_y.min(y);
            self.max_x = self.max_x.max(x);
            self.max_y = self.max_y.max(y);
        }
        self.area += 1;
    }

    fn into_contour(self) -> Contour {
        Contour {
            region: Region {
                x: self.min_x,
                y: self.min_y,
                w: self.max_x - self.min_x + 1,
                h: self.max_y - self.min_y + 1,
            },
            area: self.area,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GrayImage, NOISE_AREA_GATE};

    fn binary_from(rows: &[&str]) -> BinaryImage {
        let h = rows.len() as u32;
        let w = rows[0].len() as u32;
        let mut binary = BinaryImage::new(w, h).expect("合法尺寸");
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(row.len() as u32, w, "每行宽度必须一致");
            for (x, ch) in row.chars().enumerate() {
                binary.set(x as u32, y as u32, ch == '#');
            }
        }
        binary
    }

    #[test]
    fn two_blobs_yield_two_contours_in_scan_order() {
        let binary = binary_from(&["##...", "##...", ".....", "...##", "...##"]);
        let contours = external_contours(&binary);
        assert_eq!(contours.len(), 2);
        assert_eq!(
            contours[0],
            Contour {
                region: Region {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2
                },
                area: 4
            }
        );
        assert_eq!(
            contours[1],
            Contour {
                region: Region {
                    x: 3,
                    y: 3,
                    w: 2,
                    h: 2
                },
                area: 4
            }
        );
    }

    #[test]
    fn ring_returns_only_outer_contour_not_the_hole() {
        // RETR_EXTERNAL 语义：洞不单独返回，外轮廓包围盒覆盖整个环。
        let binary = binary_from(&["#####", "##.##", "#####"]);
        let contours = external_contours(&binary);
        assert_eq!(contours.len(), 1, "洞不得被当成独立轮廓：{contours:?}");
        assert_eq!(
            contours[0],
            Contour {
                region: Region {
                    x: 0,
                    y: 0,
                    w: 5,
                    h: 3
                },
                area: 14
            }
        );
    }

    #[test]
    fn diagonal_touching_pixels_are_one_component() {
        let binary = binary_from(&["#..", ".#.", "..#"]);
        let contours = external_contours(&binary);
        assert_eq!(contours.len(), 1, "8 连通必须合并对角像素");
        assert_eq!(
            contours[0].region,
            Region {
                x: 0,
                y: 0,
                w: 3,
                h: 3
            }
        );
        assert_eq!(contours[0].area, 3);
    }

    #[test]
    fn u_shape_merges_through_union() {
        let binary = binary_from(&["#.#", "#.#", "###"]);
        let contours = external_contours(&binary);
        assert_eq!(contours.len(), 1, "U 形必须是一个连通域：{contours:?}");
        assert_eq!(contours[0].area, 7);
    }

    #[test]
    fn empty_and_full_images_behave() {
        let binary = binary_from(&["...", "...", "..."]);
        assert!(external_contours(&binary).is_empty());

        let full = BinaryImage::from_gray_threshold(
            &GrayImage::filled(6, 4, 255).expect("合法图像"),
            0,
        );
        let contours = external_contours(&full);
        assert_eq!(contours.len(), 1);
        assert_eq!(contours[0].area, 24);
    }

    #[test]
    fn noise_gate_matches_spec_threshold() {
        let binary = binary_from(&["#.#.#"]);
        let contours = external_contours(&binary);
        assert_eq!(contours.len(), 3);
        assert!(contours.iter().all(|contour| contour.region.area() < NOISE_AREA_GATE));
    }
}
