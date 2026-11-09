//! 把 `icon.rc` 里的图标编进 exe 资源段 —— 资源管理器 / 文件属性里看到的就是它
//! （窗口内的图标走 `icon.rs`，两者同源不同用途）。
//!
//! 清单是可选的：这里只有图标，没有 manifest。

fn main() {
    println!("cargo:rerun-if-changed=icon.rc");
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    embed_resource::compile("icon.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("图标资源编译失败（找不到 rc.exe？）");
}
