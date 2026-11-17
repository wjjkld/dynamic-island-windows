//! 把 assets/island.ico 编进可执行文件，资源管理器、任务管理器里就有图标了。

fn main() {
    println!("cargo:rerun-if-changed=assets/island.rc");
    println!("cargo:rerun-if-changed=assets/island.ico");
    if let Err(e) = embed_resource::compile("assets/island.rc", embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=没能编入图标：{e}");
    }
}
