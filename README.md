# Q Switch

轻量的 **Qoder 自定义模型配置与本地路由工具**。基于 CC Switch 的 MIT 代码整理，接入方式参考 [magpie 的 Qoder 实现](https://github.com/yetone/magpie/blob/e00fbdb/internal/agent/qoder.go)。

## 核心功能

- 一个界面管理供应商、密钥和多个模型。
- Qoder 国际版 / CN 版原生 provider 配置，直接选择路由模型。
- 本地网关与流式回复，支持 Chat Completions、Responses、Anthropic Messages 上游。
- 配置备份、移除与恢复先前模型选择。
- 简单的运行状态、启停及托盘菜单。

3.19 轻量版移除了其他 AI 客户端管理、Skills / 提示词 / 会话管理、用量报表、云同步、订阅 OAuth、旧版载体映射和独立工具执行器。**工具调用与 MCP 由 Qoder 自己执行和管理权限。** 不再扫描其他客户端目录或启动相关定时任务。

## 接入原理

magpie 在 Qoder 的 `settings.json` 中注册 `providers.magpie`，将模型选择保存为 `magpie/<模型ID>`，由 Qoder 直接请求本地 OpenAI 兼容接口。

Q Switch 采用相同方式注册 `providers.qswitch`。Qoder 请求本地 `/qoder/v1/chat/completions`，Q Switch 根据稳定路由 ID 选择配置的上游，替换真实模型名与凭证，并按需要转换协议。Qoder 配置里仅保存本地入口和占位凭证 `qswitch-local`；真实上游 API Key 留在 Q Switch 配置中。

此方式要求 **支持 `settings.json` 原生 `providers` 的 Qoder CLI / App，且账号具备 BYOK 权限**。不承诺兼容旧版 Qoder IDE / Quest；轻量版已移除旧 Socket Shadow 适配器，不再使用载体模型。

## 使用

1. 启动 Q Switch，确认本地路由运行中。
2. 添加供应商，填写接口、API Key、协议和模型名。
3. 在「原生 Provider 接入」选择国际版或 CN 版，以及默认路由模型。
4. 点击「同步模型并选择」，然后在 Qoder **新建会话**。

修改模型列表、启用状态或端口后需再次同步。使用期间保持 Q Switch 运行；关闭窗口会保留托盘和路由，从托盘菜单退出才会结束应用。

国际版配置：`$QODER_CONFIG_DIR/settings.json`，默认 `~/.qoder/settings.json`。

CN 版配置：`$QODERCN_CONFIG_DIR/settings.json`，默认 `~/.qoder-cn/settings.json`。

第一次同步在相同目录保存 `settings.qswitch-native-backup.json`，只备份原来的模型选择。「移除并恢复选择」只移除 Q Switch 管理的 provider；当前仍选择 Q Switch 时恢复之前选择，用户后来选择的其他模型会保留。其他 provider、模型偏好、routing 和超时配置不会被清空。同名第三方 provider、无效 JSON 或丢失的恢复记录会报错，不会覆盖。

## 协议与限制

| 上游协议 | Base URL 自动追加路径 | 原生工具任务 |
| --- | --- | --- |
| Chat Completions | `/chat/completions` | 支持转发 Qoder 的工具协议 |
| Responses | `/responses` | 由桥接转换工具协议 |
| Anthropic Messages | `/v1/messages` | 缺少稳定会话 ID 时禁用工具 |

支持完整 endpoint 和 query 参数；选择「完整地址」时不再追加路径。模型能力依赖实际上游。当前清单不声明视觉能力。

Anthropic 的有签名思考块无法在无稳定会话 ID 的原生请求间可靠重放，因此此情形只支持对话；原生工具任务请优先使用 Chat Completions 或 Responses 上游。

真实 Qoder 客户端对话与 macOS 安装包仍需在目标机器验证，源码测试不等同于客户端端到端验证。

## 数据与迁移

继续使用默认 `~/.qswitch/cc-switch.db`，也可通过 `QSWITCH_CONFIG_DIR` 指定目录。保留旧版 `app_paths.json` 中已有的目录覆盖和旧版路由端口。

轻量版只使用供应商与设置表，保留已有其他表和其他应用记录，不会删除旧数据。旧的扁平 Qoder 路由仍可迁移成供应商与模型。上游密钥存储在本机数据库，请只使用可信上游。

## 开发和验证

Node.js 22+、pnpm 10+、Rust 1.95。桌面构建还需要对应平台的 Tauri 系统依赖。

```bash
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test:unit
pnpm build:renderer
cargo test --no-default-features --manifest-path src-tauri/Cargo.toml --lib
cargo clippy --no-default-features --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
pnpm build
```

`--no-default-features` 只编译路由和配置核心，运行真实本地 HTTP 协议测试，不需要 GTK / WebKit。默认构建包含桌面壳与托盘。CI 单独检查 macOS 桌面编译。

参考精简前后的生产构建：前端 JS 从约 4.26 MB 降到约 316 KB（约减少 93%，gzip 从约 1.26 MB 降到约 102 KB）。这不是安装包大小或内存占用测量。

## 来源与许可

Q Switch 派生自 [CC Switch](https://github.com/farion1231/cc-switch)，保留其 [MIT License](LICENSE) 和版权声明。Qoder 原生 provider wiring 的研究来源是 [yetone/magpie](https://github.com/yetone/magpie)。本项目与 Qoder、CC Switch、magpie 均无官方隶属关系。
