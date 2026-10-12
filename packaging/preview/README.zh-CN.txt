ShellRS 0.1.10-preview.1 预览测试版（Windows x64）

安装包安装到独立的 %LOCALAPPDATA%\Programs\ShellRS Preview，开始菜单快捷方式为 ShellRS Preview。
也可使用便携 ZIP：解压到单独目录，双击 launch-preview.cmd 启动。不要覆盖正式版安装目录。
窗口标题会显示 ShellRS Preview 及完整预览版本号。

隔离范围
- 主机数据库、设置、私钥文件和断点记录使用 %APPDATA%\shellrs-preview
- 预览版忽略正式版的 SHELLRS_DATA_DIR；从终端直接运行 exe 时可用 SHELLRS_PREVIEW_DATA_DIR 自定义预览目录（启动脚本固定使用上述默认目录）
- Windows 凭据管理器使用独立的 shellrs-preview 服务名，不读取或覆盖正式版 shellrs 的密码条目
- 不自动导入正式版数据或密码；请手动建立测试连接
- 预览版禁用在线更新，以及全局 CLI / Agent skill 的安装和自动更新；不会向 PATH 注册命令
- 直接运行同目录 shellrs-cli.exe 可操作预览版；不要替换已安装的全局 shellrs 命令

注意
这是未签名的试用包，不是官方发行版。请核对随包 SHA256SUMS 和构建记录。若 Windows 提示安全警告，请先确认下载来源，不要关闭系统安全防护。
本地数据隔离不隔离远端服务器：删除、上传等操作仍会影响实际连接的远端文件。建议先连接测试主机，用可恢复的测试文件验证。
GPU 信息需要远端具备相应硬件和工具；自动化验证不代表已经完成你的硬件环境验证。
预览安装器与正式版使用不同安装标识；卸载预览版不会删除预览数据或凭据，也不会卸载正式版。
退出后可继续使用原来的正式版。正式版的数据、程序和凭据不会由本包自动迁移。
