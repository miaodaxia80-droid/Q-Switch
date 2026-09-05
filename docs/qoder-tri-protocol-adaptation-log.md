# QSwitch × Qoder 三协议自定义模型适配 — 实施日志

> 目标：让 Qoder 只对接 QSwitch 一个稳定的本地 Chat 入口，由 QSwitch 按供应商配置把
> canonical OpenAI Chat 请求转换到三种上游协议之一：
> `openai_chat`（Chat Completions）、`anthropic_messages`（Anthropic Messages）、
> `openai_responses`（Responses），并把三种上游响应/流式事件统一转回 Qoder Chat。
>
> 约定：本文件按 P0→P3 记录每一步“改了什么、为什么、如何验证”。代码内同步加 `log::*` 运行时日志。

## 基线（2026-09-05）

- 分支 `feature/qoder-support`，HEAD `2a83042`，包版本 3.18.1。
- 工作树已有用户改动：`CHANGELOG.md`、`docs/guides/qoder-routing-guide-zh.md`，未跟踪 `scripts/e2e-qoder-custom-route/`，本轮全部保留、不覆盖。
- 现状结论（已逐文件核对）：
  - Qoder 入口仅 `/qoder/chat/completions`、`/qoder/v1/chat/completions`（`proxy/server.rs`）。
  - `handle_qoder_chat_completions` 固定按 Chat Completions 解析并走统一 forwarder（`proxy/handlers.rs`）。
  - `QoderCustomRoute` 只有 model/base_url/api_key/reasoning_effort，无 api_format/is_full_url（`qoder_config.rs`）。
  - 自定义路由落 `qoder_custom_routes_v1`，并镜像成隐藏 `qoder` provider。
  - manifest 的 route id 由 `sha256(provider.id + model)` 派生，改上游模型名会失配；`protocol_type` 硬编码 `"openai"`。
  - ACP `chat_client.rs` 固定 POST `/chat/completions`，未把稳定 ACP `sessionId` 传给网关。
  - ACP `handler.rs` 工具轮数硬编码 8。
  - 前端 `CustomRoutePanel.tsx` 的 Radix `SelectItem value=""` 有空值风险。


## P1-a 新增协议桥 `src-tauri/src/proxy/providers/qoder_wire.rs`（已完成，单测通过）

- `QoderApiFormat`：`openai_chat` / `anthropic_messages` / `openai_responses`，含解析、路径后缀、UI label。
- `build_upstream_endpoint`：Base URL 模式按格式追加 `/chat/completions`、`/v1/messages`、`/responses`；
  完整地址模式不追加；仅允许 http/https，拒绝 userinfo、fragment、空 host；用 `url` crate 在 path 上
  追加以保证显式 `?query` 留在末尾；`redact_endpoint_for_log` 只记录 scheme/host/path。
- `apply_upstream_auth`：Chat/Responses 用 `Authorization: Bearer`，Anthropic 用 `x-api-key` +
  `anthropic-version`（默认 2023-06-01）；拒绝 CR/LF/NUL 注入。
- 请求转换：`chat_to_anthropic`（system 提顶、tool_calls→tool_use、role=tool 合并为单个 user 的
  tool_result、function.parameters→input_schema、tool_choice auto/any/tool、max_tokens 归一）；
  `chat_to_responses`（system→instructions、消息→input items、function_call/function_call_output、
  tools、tool_choice、max_output_tokens、reasoning.effort）。
- 非流式：`anthropic_to_chat` / `responses_to_chat`，统一 usage（input/prompt、output/completion）。
- 流式状态机：`AnthropicSseTranslator`、`ResponsesSseTranslator`，输出 Chat SSE：
  文本增量、并行工具调用、参数分片、finish_reason（tool_calls/length/stop）、usage、[DONE]；
  Anthropic thinking 文本转 `reasoning_content` 供 UI 展示，签名块存入桥接状态供下一轮重放。
- 桥接状态：进程内 `BRIDGE_STORE`（Mutex<HashMap>，TTL 1h），只存 Anthropic message id、
  tool_use→signed thinking、`bridge_take_thinking`/`bridge_drop_session`/`bridge_sweep`，
  不存 prompt/完整回复/密钥。
- `bridge_qoder_upstream`：reqwest 发送上游，非流转 JSON、流转 SSE；上游错误只回 HTTP 状态码+
  脱敏类别+request id，不回显完整 body；断流产出收尾帧。
- 单测 18 个：枚举解析、URL 拼接/query/非法输入、脱敏、三种认证头、Chat→Anthropic、Chat→Responses、
  两种非流式、SSE 拆分、Anthropic/Responses 文本与工具分片流、thinking 存取重放、usage。
- 验证：`cargo test --lib qoder_wire` 全绿（当前 22 个测试，含 4 个 loopback 契约测试）。

## P0 数据模型重构 + P1 网关接入（已完成，qoder 后端单测全绿）

### 数据模型（`qoder_config.rs`）
- 新增 `QoderCustomProvider`（id/name/baseUrl/apiKey/apiFormat/isFullUrl/anthropicVersion/enabled），
  密钥落本地库但 list/save 返回前用 `mask_custom_provider` 脱敏（只给 hasApiKey）。
- `QoderCustomRoute` 演进为“供应商下的模型”：新增 providerId/enabled，baseUrl/apiKey 仅作旧数据迁移字段。
- 稳定 route id：`stable_qoder_route_id = qswitch_<provider_uuid>_<model_uuid>`，改上游模型名不再失配；
  manifest 优先用 catalog 里的 `routeId`，非自定义 qoder provider 才回退哈希。
- manifest 条目与 `QoderRouteTarget` 新增 `apiFormat`/`isFullUrl`；对 Qoder 侧 `type` 仍恒为 `openai`。
- 迁移 `ensure_custom_model_migration`：旧扁平 route → 每 route 一个 provider+一个模型；用旧哈希公式
  重算旧 route id 并重写 carrier 映射；旧设置先备份到
  `qoder_custom_routes_v1_pre_triprotocol_backup`，待新设置和镜像全部成功后再删除旧隐藏 provider；失败时保留旧数据；幂等。
- 供应商/模型 CRUD：save/delete provider（级联模型、隐藏镜像、相关 carrier 映射）、save/delete model
  （重镜像父供应商）；供应商地址在保存时用 qoder_wire 的 URL 校验，apiFormat 用枚举解析。
- 命令层新增 `qoder_list/save/delete_custom_provider` 并在 lib.rs 注册。

### 网关接入（`proxy/handlers.rs`）
- `handle_chat_completions_for_app` 在解析 Qoder route 后分流：
  - openai_chat + Base URL：保持原 forwarder 路径（不回归）；
  - anthropic/responses，或 chat 的完整地址模式：走 `qoder_wire::bridge_qoder_upstream`。
- 从内部头 `x-qswitch-qoder-session-id` 取稳定会话；Anthropic 且无会话时降级为无工具轮并告警。

### 稳定会话（`qoder_acp/chat_client.rs`、`handler.rs`）
- `stream_chat_completion` 新增 session_id 参数，向网关发 `x-qswitch-qoder-session-id`。
- ACP `session/close`：移除会话并 `bridge_drop_session` 清理桥接状态。

### 验证
- `cargo test --lib qoder`：当前 69 passed / 0 failed / 5 ignored（含迁移、稳定 id、provider 镜像、协议桥全套）。

## P2 前端供应商+多模型、契约测试、E2E 脚本加固（已完成代码与自动验证）

### 前端
- `src/lib/api/qoderRoute.ts`：新增 `QoderApiFormat`、格式标签/选项、`QoderCustomProvider`，
  `QoderCustomRoute` 改为“供应商下的模型”（providerId/enabled），新增 list/save/delete custom provider。
- `CustomRoutePanel.tsx` 重写为“供应商 + 多模型”：
  - 供应商级：名称、API 格式（三选）、地址类型（Base URL/完整请求地址）、地址（按格式提示自动追加路径）、
    API Key（编辑时留空不变、可显式清除）、anthropic-version（仅 Anthropic 时显示）；
  - 供应商内可增删多个模型行（显示名、上游模型名、思考强度、上下文窗口、推理开关）；
  - 保存时先存供应商拿到稳定 id，再 reconcile 模型（新增/更新/删除移除项），最后同步 manifest；
  - 修复 Radix `SelectItem value=""` 运行时风险：思考强度用 `default` 哨兵，地址类型用 `base/full`，
    API 格式恒为非空枚举。
- `QoderRoutePanel.tsx` 工具策略区加 QSwitch 策略提示：单任务最多 8 轮工具（区别于 Qoder 官方 500 轮），
  自定义端点不会因协议获得更高权限。
- `pnpm run typecheck`（tsc --noEmit）通过。

### 协议桥契约测试（真实 loopback socket）
- `qoder_wire.rs` 新增 4 个 `#[tokio::test]`：一次性 mock 记录请求并回对应 SSE，
  分别验证 Chat 透传、Anthropic 双向转换、Responses 双向转换、上游 401 转为同状态码脱敏错误体；
- 断言点：上游 path 后缀、认证头（Bearer / x-api-key）、请求体结构（messages/system+max_tokens/input）、
  真实模型名、回转 Chat SSE 含 chat.completion.chunk/文本/[DONE]；
- 安全断言：内部 `x-qswitch-qoder-session-id` 不转发到真实上游；401 上游响应体不被回显。
- `cargo test --lib qoder_wire`：22 passed / 0 failed。

### E2E 脚本加固（scripts/e2e-qoder-custom-route）
- mock_openai.py：同一端口服务 /chat/completions、/v1/messages、/responses 三种 SSE；
  只记录请求头“名称”与认证头是否存在，绝不记录 Key 值；记录 model/stream/tools/tool_choice/reasoning/session 头。
- inject.py：固定测试 UUID；provider/model/mapping 一律“合并进现有数组”而非整盘覆盖；
  snapshot 阶段保存 QSwitch 设置、Qoder `aicoding.customModels` 和 manifest 原文；cleanup 在有快照时精确恢复，
  无快照时不碰 Qoder DB，manifest 若原本不存在则只剔除测试 provider/route 项；测试 Key 为无权限占位值。
- run.sh：`trap cleanup EXIT INT TERM` 兜底恢复；先 snapshot 再注入；按 QSWITCH_E2E_API_FORMAT 断言对应路径。
- python 与 bash 语法校验通过。

## 全量验证结果（P3 自动化部分）

- `cargo check`：0 error，本次改动文件 0 warning。
- `cargo test --lib qoder`：69 passed / 0 failed / 5 ignored。
- `cargo test --lib qoder_wire`：22 passed（含 4 个 loopback 三协议契约测试 + 401 脱敏测试）。
- `cargo test --lib proxy`：1252 passed，6 failed —— 当前失败仍是同一组全局代理端口隔离断言（15721↔15731）；
  已用 `git stash` 在干净 HEAD `2a83042`
  上复现同样 6 个失败（claude_desktop_config / services::proxy 的全局代理端口 15721↔15731
  进程级 OnceCell 测试隔离问题），确认为**历史遗留、与本次改动无关**。
- `pnpm run typecheck`（tsc --noEmit）：通过。
- `pnpm run test:unit`（vitest）：81 files / 532 tests 全通过。
- `pnpm run build:renderer`（vite build）：构建成功（仅原有的 chunk 体积提示）。
- `git diff --check`：无空白错误。
- CHANGELOG：在保留既有 3.18.1 段落前提下新增 `[Unreleased] - Qoder three-protocol custom providers`。

### 仍需人工/真机完成（无法在本环境自动化）
- 版本号定版与“独立测试安装包”构建（避免覆盖已装 3.18.0）：当前代码版本仍为 3.18.1，
  发布前再统一改 package.json / Cargo.toml / tauri.conf.json 并 `pnpm tauri build`。
- Qoder 1.28.0 真机 Quest 三协议验收（按方案第 7 节 16 步），依赖本机 Qoder UI 操作。
- 工具轮数维持 8 轮（未改 handler.rs 的 8 轮上限），升级到 500 属独立安全/成本任务。

### 收尾清理
- 消除本次新增代码的全部 dead-code 告警：两个 SSE 翻译器真正使用 `model` 字段
  （每个转出的 Chat chunk 都带 model），移除未用的 `label`，仅测试使用的两个格式常量加 `#[cfg(test)]`。
- 最终 `cargo check` 对 qoder/handlers 相关文件 0 warning；`cargo test --lib qoder` 69 passed。
