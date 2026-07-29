# Q Switch

> A derivative version of CC Switch that adds local routing and custom-model adaptation for Qoder Native BYOK, with MCP and opt-in tool support.

**Q Switch 是 [CC Switch](https://github.com/farion1231/cc-switch) 的衍生版本。**它在 CC Switch 既有能力的基础上，重点新增了 Qoder 原生 BYOK 的本地路由、载体模型映射，以及用户显式授权的 MCP 与工具调用适配。

它保留 Qoder 原生 BYOK 的模型选择界面，并在本机把**已明确映射**的载体模型请求路由到你在 Q Switch 中配置的自定义模型上游。

项目不修改 Qoder 的安装包、Renderer 或扩展文件。适配器只在本机运行，并对未映射的 Qoder 会话透明转发。

> 本项目仅用于个人技术研究与学习。Qoder、CC Switch 及其商标分别归其权利人所有；本项目与它们均无隶属或官方关系。

## 与 CC Switch 的关系

- Q Switch 以 CC Switch 为上游代码基础，保留 [MIT License](LICENSE) 和原版权声明。
- 本分支的主要新增内容是 **Qoder 适配层**：原生 BYOK 载体模型映射、本地 Socket Shadow 传输适配器、Qoder 自定义模型路由，以及受用户开关控制的 MCP / 文件 / 终端 / 网络工具能力。
- Q Switch 不是 CC Switch 官方发布版，也不是 Qoder 官方插件或官方模型提供商。

## 当前能力

已在真实 Qoder Quest UI 和 Q Switch 本机审计中完成受控回归的能力：

| 能力 | 状态 | 说明 |
| --- | --- | --- |
| 自定义模型路由 | 已验证 | Qoder 原生 BYOK 载体模型可映射到 Q Switch 已配置路由 |
| SSE / 流式回复 | 已验证 | 路由在本机进行，密钥不写入 Qoder 模型清单 |
| 工作区写入 | 已验证 | 需要在 Q Switch 中显式开启 |
| 终端 | 已验证 | 需要显式开启；以当前系统用户身份执行，并非沙箱 |
| 公共网络 | 已验证 | 需要显式开启；默认阻止 localhost、内网与链路本地地址 |
| MCP | 已验证 | 支持 stdio 和 Streamable HTTP；对应服务器还须在 MCP 面板中启用 Qoder |
| 私有网络 | 默认关闭 | localhost、局域网与内网请求不会被授予 |

完整的设计和操作说明见 [Qoder 路由指南](docs/guides/qoder-routing-guide-zh.md)。

## 详细使用流程

Qoder 里的模型和 Q Switch 里的模型承担不同角色：

- **Qoder 原生 BYOK 模型**是让 Qoder 正常创建和选择自定义模型的**载体模型**。例如，可先在 Qoder 中添加它原生支持的阿里云百炼模型。
- **Q Switch 路由模型**才是映射后实际接收 Quest 请求的目标上游模型；它可以和载体模型不同。

不要把 Q Switch 的上游密钥填入 Qoder，也不要把 Qoder 的 BYOK 密钥复制到 Q Switch。两组密钥各自只留在本机对应应用中。

### 1. 先在 Qoder 创建载体模型

1. 安装并启动 Qoder，完成正常登录和 BYOK 前置设置。
2. 在 Qoder 的自定义模型 / BYOK 页面，添加一个 **Qoder 原生支持**的提供商和模型；阿里云百炼只是其中一个示例。
3. 保存该自定义模型。保存后，Qoder 会为它创建一个本机 `custom:<record_id>` 载体记录。

这一步只是在 Qoder 中创建载体，**不需要先发送真实任务对话**。

### 2. 配置 Q Switch 的实际目标模型

1. 启动 `Qswitch.app`。首次打开若被 Gatekeeper 拦截，请在 Finder 中右键应用并选择“打开”。
2. 在 Q Switch 中添加实际要调用的提供商、地址、密钥和目标模型。
3. 打开 **Qoder** 页面，确认本地路由可用，并启用“原生传输适配器”。
4. 若 Qoder 此时已经打开，完全退出后重新启动 Qoder，使其原生 Agent 重新连接适配器。

### 3. 观察载体 ID 并保存映射

1. 在 Qoder 新建 Quest，选择刚才创建的原生 BYOK 载体模型。
2. 回到 Q Switch 的 **Qoder** 页面，点击刷新；面板会显示本次选择实际使用的载体模型 ID。
3. 选择要转发到的 Q Switch 路由，点击“保存映射”。
4. 回到 Qoder，重新选择该载体模型，或新建一个 Quest 后再次选择它。

未保存映射的载体会 fail-closed：Q Switch 不会把它悄悄转发到任意上游。因此，应先完成映射，再发送真实任务。

### 4. 发起首次验证对话

1. 在新 Quest 中确认当前选择的仍是已映射载体模型。
2. 先发送一个不含私密文件、密钥或个人数据的简单只读请求。
3. 在 Q Switch 日志确认本次会话出现载体映射和本地路由记录；这证明请求进入了 Q Switch。

要使用写文件、终端、公共网络或 MCP，请在 Q Switch 的 Qoder 页面单独打开对应权限。私有网络保持关闭，除非你明确理解风险且确实需要本地服务。

## 验证安装包

在包含应用的目录执行：

```bash
codesign --verify --deep --strict --verbose=2 Qswitch.app
file Qswitch.app/Contents/MacOS/qswitch
```

当前受控回归的成功判据不是模型回复文本，而是同时满足：

1. Qoder Quest 中出现相应工具步骤；
2. Q Switch 日志出现该会话的载体映射；
3. Q Switch 日志出现 `tool completed kind=<类型> success=true`。

若使用 MCP 且首次发现超时，先确认 Qoder 已完全启动、MCP 服务器的命令在目标机可执行，然后新建 Quest 重试。不要仅因模型输出了预期文字就视为工具已执行。

## 安全边界

- 上游模型密钥、MCP 密钥及工作区内容属于敏感数据；请只接入可信上游和 MCP 服务器。
- 终端能力以当前 macOS 用户身份运行，不提供系统级沙箱。
- Q Switch 只接管已映射载体模型；官方模型和未映射的原生 BYOK 请求透明转发。
- 停用适配器会恢复 Qoder 原生本机入口，不会修改 Qoder 应用包。

## 从源码构建

```bash
pnpm install
pnpm run build
```

构建产物位于：

```text
src-tauri/target/release/bundle/macos/Qswitch.app
```

发布前建议至少执行：

```bash
pnpm run typecheck
cargo test --manifest-path src-tauri/Cargo.toml qoder_acp::mcp_runtime::tests::stdio_mcp_discovery_and_call_use_the_protocol_handshake --lib
cargo clippy --manifest-path src-tauri/Cargo.toml --lib -- -D warnings
codesign --verify --deep --strict --verbose=2 src-tauri/target/release/bundle/macos/Qswitch.app
```

## 发布方式

当前 Q Switch 只发布经过本机验证的 Apple Silicon（arm64）macOS 测试包。为避免继承自 CC Switch 的跨平台签名、公证工作流在没有对应证书时产生误导性失败，**推送 Git 标签不会自动构建或发布**。

发布者应在本机完成构建与校验后，创建 GitHub Release 并上传 DMG：

```bash
hdiutil verify src-tauri/target/release/bundle/dmg/Qswitch_3.18.0_aarch64.dmg
shasum -a 256 src-tauri/target/release/bundle/dmg/Qswitch_3.18.0_aarch64.dmg
gh release create v<版本>-qswitch.<序号> \
  src-tauri/target/release/bundle/dmg/Qswitch_3.18.0_aarch64.dmg \
  --repo miaodaxia80-droid/Q-Switch --prerelease
```

发布说明中应注明目标架构、macOS 最低版本、是否经过 Apple Developer ID 公证，以及对应的 SHA-256。若将来配置 Tauri 更新签名、Apple Developer ID 证书和公证凭据，再单独引入自动化发布流程。

## 许可证与来源

Q Switch 派生自 [CC Switch](https://github.com/farion1231/cc-switch)，保留其 [MIT License](LICENSE) 及版权声明。对本分支的修改同样按 MIT License 提供。
