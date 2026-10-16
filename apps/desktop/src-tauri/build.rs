fn main() {
    // Tauri 的构建脚本：生成上下文、校验配置。`bundle.active = false`
    // （见 tauri.conf.json），因为本项目的 CI 不构建安装包 —— 那是发布流程的事，
    // 不是"代码是否正确"的事。
    tauri_build::build();
}
