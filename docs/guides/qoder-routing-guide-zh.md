# Qoder 原生 BYOK + Q Switch 本地路由

> 实验性功能开发记录；最后更新：2026-09-05。

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
        │ WebSocket 或 Unix IPC（端点均来自 .info.json）
        ▼
Q Switch 发现记录接管适配器
        ├── 官方模型、未映射 BYOK ──► 原生 Qoder Agent（透明转发）
        └── 已映射载体模型 ──────────► Q Switch /qoder/v1
                                             │
                                             ▼
                                   已配置 provider / 自定义模型
```

Qoder 的 Electron 组件从 `.info.json` 读取本机 Agent 端点（WebSocket 端口与 Unix IPC 路径）。适配器在该文件中发布自己的回环端点并保留原生坐标（`qswitchAdapter` 标记块），所有不属于映射会话的 ACP/LSP JSON-RPC 帧原样转发给原生 Agent。原生 Agent 会周期性刷新 `.info.json`（实测约每 30 秒），适配器的监视器在一个检查周期内收回记录并跟随最新原生端点；这样既不修改 Qoder 安装包，也不会被原生刷新绕开。

载体模型 ID 只从同一 ACP 会话中观察，未映射的自定义载体会 fail-closed：模型选择能同步到原生 Agent，但随后 prompt 会在本机拒绝，直到用户在 Q Switch 保存映射并重新选择载体。最近观察到的未映射 ID 只保存在进程内存，不记录提示词、回复或密钥。

## 使用顺序

1. 在 Q Switch 的「Qoder」页面确认本地路由可用，并配置好目标 provider/模型。
2. 在 Qoder 原生 BYOK 中选择一次载体模型。
3. 回到 Q Switch，将显示的载体 ID 映射到目标 Q Switch 路由并保存。
4. 启用「原生传输适配器」，然后**重启 Qoder（或新开一个 Quest 窗口）**，再重新选择该载体后新建 Quest。
5. 用 Q Switch 的本地日志确认真实请求已映射。模型清单、健康检查或“适配器已启用”都不构成端到端通过。

> 关键：Qoder 的 Electron 窗口只会在**新建连接**时重新读取 `.info.json`。
> 如果 Qoder 已在适配器启用之前连上原生 Agent，窗口会一直保持那条原生连接，
> 之后的模型选择会绕过适配器，面板就“抓不到载体模型”。
> 面板新增了「Qoder 是否已通过适配器连接」的状态：只要适配器启用后 Qoder
> 还没有任何连接经过适配器，就会给出提示，请先重启 Qoder 或新开 Quest 窗口。

适配器关闭、Q Switch 正常退出时都会把 `.info.json` 恢复为最新的原生记录，Qoder 不需要重装。若 Q Switch 异常退出（崩溃或强杀），下次启动时会自动回收仍归已死进程所有的适配器记录，Qoder 随之恢复直连原生 Agent。若 Qoder 更新，先关闭适配器，再重新验证 ACP 帧形状与发现记录生命周期。

## 自定义模型（任意模型名 + 任意请求地址）

Qoder 原生 BYOK 的 provider 只能选 Qoder 内置渠道，请求也由 Qoder 云端代发。
自定义模型路由让 Q Switch 的透明适配器把请求**直连**到你自己的 HTTP(S) 模型端点：

- **模型名完全自由**：发给上游的 `model` 字段就是你在自定义模型里填的模型名；
- **请求地址完全自由**：`base_url` 可以是任意 HTTP(S) 地址（本地或公网），
  Q Switch 按所选的 Chat Completions、Anthropic Messages 或 Responses 协议发送，
  API Key 只存在 Q Switch 本地数据库；
- **思考强度可调**：每条自定义路由可选 `reasoning_effort`（low / medium / high），
  网关转发前注入请求体，覆盖 Qoder 客户端的值。

### 使用顺序

1. Q Switch →「Qoder」页面 →「自定义模型供应商」→「添加模型供应商」：
   填供应商名称、Base URL 或完整请求地址、API Key（可选）、三选一的 API 格式，
   再在供应商下添加一个或多个模型（显示名、上游模型名、思考强度、上下文窗口、
   是否支持推理）。保存后自动刷新 Qoder 模型清单。
2. 按上方「使用顺序」在 Qoder 添加并选择一次 BYOK 载体，回到 Q Switch 把该载体
   映射到这条自定义路由（映射下拉里会显示自定义模型的名称）。
3. 启用「原生传输适配器」，重启 Qoder（或新开 Quest 窗口），重新选择载体后
   新建 Quest，请求即直达自定义地址。

### 注意

- 自定义路由与 Qoder 内置渠道完全解耦，不需要在 Qoder 里有对应的 provider；
- 地址只接受 `http://` 或 `https://`，拒绝 URL 中的用户信息和 fragment；Base URL 会按格式补路径，
  完整地址模式原样请求；当前版本不提供自定义请求头或自定义 JSON 模板。
- 测试自定义地址时，若地址是本机端口（如 API 网关），注意本机系统代理
  （`ERR_NO_SUPPORTED_PROXIES` 之类）可能影响公网请求，但不影响 127.0.0.1；
- 删除自定义模型会同步删除其隐藏 provider 并刷新清单，已保存的载体映射若指向
  已删除路由会显示为“已失效”。

## 排障：面板观察不到载体模型

1. 确认面板显示「Qoder 已通过适配器连接」；若显示「尚未通过适配器连接」，
   说明 Qoder 窗口仍在直连原生 Agent。重启 Qoder（或新开一个 Quest 窗口）
   后再选择一次载体模型。
2. 确认选择动作发生在适配器启用之后；启用之前选择过的模型 ID 不会补录。
3. 确认 `.info.json` 里的 `websocketPort`/`ipcServerPath` 属于 Q Switch
   （`ipcServerPath` 应以 `/tmp/qswitch-qoder-` 开头）。若仍是原生记录，
   说明适配器没有在运行，或上次异常退出留下的失效记录尚未被回收。
4. 若 Q Switch 异常退出过：重新启动 Q Switch，启动时会自动回收失效的适配器记录，
   再按第 1 步让 Qoder 重新连接。

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
- 曾评估过只 shadow 原生 Unix socket 的传输方案：Qoder Electron 主进程按 `.info.json` 的 WebSocket 端点连接，Quest 流量会完全绕过适配器，面板观察不到载体模型。该方案已从构建中移除；发现记录接管 + 周期刷新收回是当前唯一维护的传输路径。

## 相关文件

- [发现记录接管代理](../../src-tauri/src/qoder_acp/proxy.rs)
- [Qoder 工具与权限](../../src-tauri/src/qoder_acp/tools.rs)
- [MCP 运行时](../../src-tauri/src/qoder_acp/mcp_runtime.rs)
- [工具审计](../../src-tauri/src/qoder_acp/handler.rs)
- [Qoder 路由面板](../../src/components/qoder/QoderRoutePanel.tsx)
- [应用路由总说明](../user-manual/zh/4-proxy/4.2-routing.md)
