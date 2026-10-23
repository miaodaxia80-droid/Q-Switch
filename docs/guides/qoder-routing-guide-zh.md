# Qoder 原生模型路由指南

适用于 Q Switch 3.19 轻量版。完整使用、恢复和构建说明见 [README](../../README.md)。

## 配置顺序

1. 在 Q Switch 添加实际供应商与模型，启动本地路由。
2. 选择 Qoder 国际版或 CN 版，以及一个默认路由模型。
3. 点击「同步模型并选择」。
4. 在 Qoder 新建会话。旧会话可能保留原来的配置。

Qoder 使用的模型选择值是 `qswitch/<稳定路由ID>`，发送给本地入口的模型值是稳定路由 ID。本地网关将其解析为实际供应商和模型。修改上游模型名不会改变路由 ID。

## 恢复

「移除并恢复选择」移除 `providers.qswitch`，并在当前仍选择 Q Switch 时恢复备份的原模型选择。不会覆盖用户后来手动选择的其他模型，也不会清空其他 provider 或偏好。同目录的 `settings.qswitch-native-backup.json` 是恢复所需记录，不要在启用期间删除。

## 故障排查

- 无法启动：检查本地端口是否被其他进程占用。
- 看不到模型：确认 Qoder 版本支持原生 `providers`，账号具备 BYOK 权限；再次同步后新建会话。
- 未知路由：路由可能已删除或禁用；重新启用、同步并新建会话。
- 上游 401 / 404：核对 API Key、协议、Base URL / 完整地址模式。
- Anthropic 模型不执行工具：原生请求缺少稳定会话 ID 时会禁用工具，改用 Chat Completions 或 Responses 上游。

轻量版不再提供旧 Socket Shadow 载体适配器；Qoder 自己管理 MCP、工具权限、工作区和会话。此前旧版 Quest 的验证结论不代表新版原生接入已完成真实客户端验证。
