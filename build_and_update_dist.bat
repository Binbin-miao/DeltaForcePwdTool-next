@echo off
chcp 65001 >nul
echo [1/2] 正在编译最新 Release 版本 (cargo build --release)...
cargo build --release
if %errorlevel% neq 0 (
    echo [错误] 编译失败，请检查 Rust 工具链环境。
    pause
    exit /b %errorlevel%
)
echo [2/2] 正在更新 dist\delta-pwd-tool.exe ...
copy /Y "target\release\delta-pwd-tool.exe" "dist\delta-pwd-tool.exe"
echo [完成] dist\delta-pwd-tool.exe 已更新为最新编译产物！
pause
