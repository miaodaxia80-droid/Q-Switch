# Qoder 原生 BYOK + Q Switch 本地路由

> 实验性功能开发记录；最后更新：2026-07-29。

## 当前状态

Q Switch 保留 Qoder 原生 BYOK 的模型选择界面，通过本机传输适配器把**显式映射**的载体模型路由到 Q Switch 已配置的自定义上游。Qoder 安装包、Renderer 与扩展均不修改。

已由真实 Qoder Quest UI 和 Q Switch 审计日志共同确认的能力：

| 能力 | 当前结论 | 受控范围 |
| --- | --- | --- |
| 映射后的模型请求 | 已通过 | Qoder Quest 请求到达 Q Switch，并走已选本地路由 |
| 工作区写入 | 已通过 | 仅当前 Qoder 工作区内的受控测试文件 |
| 终端 | 已通过 | 当前工作区、固定无副作用命令 |
| 公网 HTTP | 已通过 | 仅凭证外的公开站点 HEAD 请求 |
| MCP stdio | 已通过 | 单个无文件/网络/命令副作用的 echo 夹具 |
| 私有网络 | **保持关闭** | localhost、内网和链路本地地址未授予 |

MCP 的已通过回归包含明确 Node 路径和裸 `node` 两种配置；macOS GUI 下裸 `node` 会映射到当前架构的标准 Node 位置。两种配置均已由真实 Quest 与审计日志确认，不能只看模型回复文本。

## 设计

```text
Qoder Quest / Electron
        │ 原生 Unix IPC（兼容 WebSocket）
        ▼
Q Switch Socket Shadow 传输适配器
        ├── 官方模型、未映射 BYOK ──► 原生 Qoder Agent（透明转发）
        └── 已映射载体模型 ──────────► Q Switch /qoder/v1
                                             │
                                             ▼
                                   已配置 provider / 自定义模型
```

适配器会临时把 Qoder 原生 Unix socket 移到私有 shadow 路径，在原路径监听；所有不属于映射会话的 ACP/LSP JSON-RPC 帧原样转发。这样避免改写 Qoder 安装包，也避免依赖会被原生 Agent 刷新的发现文件替换。

载体模型 ID 只从同一 ACP 会话中观察，未映射的自定义载体会 fail-closed：模型选择能同步到原生 Agent，但随后 prompt 会在本机拒绝，直到用户在 Q Switch 保存映射并重新选择载体。最近观察到的未映射 ID 只保存在进程内存，不记录提示词、回复或密钥。

## 使用顺序

1. 在 Q Switch 的「Qoder」页面确认本地路由可用，并配置好目标 provider/模型。
2. 在 Qoder 原生 BYOK 中选择一次载体模型。
3. 回到 Q Switch，将显示的载体 ID 映射到目标 Q Switch 路由并保存。
4. 启用「原生传输适配器」，然后重启 Qoder；重新选择该载体后新建 Quest。
5. 用 Q Switch 的本地日志确认真实请求已映射。模型清单、健康检查或“适配器已启用”都不构成端到端通过。

适配器关闭时恢复它自己接管的 socket；Qoder 不需要重装。若 Qoder 更新，先关闭适配器，再重新验证 ACP 帧形状和 socket 生命周期。

## 工具权限与安全边界

默认只允许只读工作区工具。写入、终端、公网和 MCP 均是本机用户逐项开启的许可；关闭时对应函数不会暴露给上游模型。

- 写入仅允许当前工作区内的 UTF-8 文件，拒绝绝对路径、`..` 与符号链接逃逸。
- 终端以当前 macOS/Windows 用户运行，工作区只是起始目录，**不是**操作系统级沙箱。
- 网络仅允许 HTTP(S) 的 GET/HEAD/POST，并默认拒绝回环、私网与链路本地地址。
- MCP 还需在「工具 / MCP」为具体服务器勾选 Qoder。stdio 与 Streamable HTTP 已支持；SSE-only MCP 不投影。
- 私有网络保持单独关闭，只有用户明确需要本地服务时才应开启。

不要给不受信任的上游模型或 MCP 服务器开启终端、写入或 MCP。Q Switch 只监听本机回环地址；真实 API Key 仅保留在 Q Switch 本地配置，不写入 Qoder、日志或本文件。

## 验收方法

高级工具必须同时满足以下条件才算通过：

1. Qoder Quest 中出现相应工具的完成状态；
2. Q Switch 日志出现本次会话的载体映射；
3. Q Switch 日志出现 `tool completed kind=<类型> success=true`。

模型回复中出现预期文字本身不构成工具调用证据。尤其是 MCP：若日志出现 `MCP stdio request timed out`，应视为未通过，检查 Node 命令、服务器配置与 GUI 启动环境后再测。

开发用的无副作用 stdio MCP 夹具位于 [qswitch-qoder-regression-mcp.mjs](../../scripts/qswitch-qoder-regression-mcp.mjs)，仅提供 echo 工具，不读取文件、不执行命令、不联网。

## 验证与发布记录

- Rust Qoder 模块测试通过；需要本机回环权限的 socket 集成测试保持 `ignored`，不能记作已通过。
- `cargo clippy --lib -- -D warnings`、`pnpm run typecheck` 与 `git diff --check` 已通过。
- macOS 应用包完成 ad-hoc 签名并通过 `codesign --verify --deep --strict`。这适用于本机开发测试，不是 Developer ID 公证发布。
- 以前直接改写 `.info.json` 的发现记录方案已证伪：原生 Agent 会刷新该文件。当前 socket-shadow 方案是唯一继续维护的传输路径。

## 相关文件

- [Socket-shadow 代理](../../src-tauri/src/qoder_acp/proxy.rs)
- [Qoder 工具与权限](../../src-tauri/src/qoder_acp/tools.rs)
- [MCP 运行时](../../src-tauri/src/qoder_acp/mcp_runtime.rs)
- [工具审计](../../src-tauri/src/qoder_acp/handler.rs)
- [Qoder 路由面板](../../src/components/qoder/QoderRoutePanel.tsx)
- [应用路由总说明](../user-manual/zh/4-proxy/4.2-routing.md)
