import { useState } from "react";
import { toast } from "sonner";
import { qoderRouteApi, type QoderRouteOption } from "@/lib/api/qoderRoute";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

export function NativeProviderPanel({
  routes,
  isRunning,
}: {
  routes: QoderRouteOption[];
  isRunning: boolean;
}) {
  const [target, setTarget] = useState<"qoder" | "qoder-cn">("qoder");
  const [routeId, setRouteId] = useState("");
  const [busy, setBusy] = useState(false);
  const selected = routes.some((route) => route.routeId === routeId)
    ? routeId
    : "";

  const apply = async (remove: boolean) => {
    setBusy(true);
    try {
      const path = remove
        ? await qoderRouteApi.removeNativeProvider(target)
        : await qoderRouteApi.syncNativeProvider(target, selected);
      toast.success(
        remove
          ? `已移除 Q Switch 原生 provider：${path}。请新建会话。`
          : `已同步模型并保存选择：${path}。请在 Qoder 新建会话。`,
      );
    } catch (error) {
      toast.error(`Qoder 原生配置失败：${String(error)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-3 rounded-lg border border-border-default p-3">
      <p className="text-sm font-medium">
        原生 Provider 接入（Qoder CLI / App）
      </p>
      <p className="text-xs text-muted-foreground">
        将已启用的 Q Switch 路由同步到 Qoder 的原生模型列表，直接选择实际模型。
        适用于支持 settings.json providers 的 Qoder CLI，以及共用 CLI 配置的
        Qoder App。需要 Qoder 账号已开通 BYOK。
      </p>
      <div className="grid gap-2 md:grid-cols-2">
        <Select
          value={target}
          onValueChange={(value) => setTarget(value as "qoder" | "qoder-cn")}
          disabled={busy}
        >
          <SelectTrigger aria-label="Qoder 原生配置目标">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="qoder">Qoder 国际版（~/.qoder）</SelectItem>
            <SelectItem value="qoder-cn">Qoder CN（~/.qoder-cn）</SelectItem>
          </SelectContent>
        </Select>
        <Select
          value={selected}
          onValueChange={setRouteId}
          disabled={busy || routes.length === 0}
        >
          <SelectTrigger aria-label="原生默认模型">
            <SelectValue placeholder="选择默认路由模型" />
          </SelectTrigger>
          <SelectContent>
            {routes.map((route) => (
              <SelectItem key={route.routeId} value={route.routeId}>
                {route.displayName}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="flex gap-2">
        <Button
          size="sm"
          disabled={busy || !isRunning || !selected}
          onClick={() => void apply(false)}
        >
          同步模型并选择
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={busy}
          onClick={() => void apply(true)}
        >
          移除并恢复选择
        </Button>
      </div>
      <p className="text-[11px] text-muted-foreground">
        同步会更新所选版本的 settings.json，并备份原来的模型选择；支持
        QODER_CONFIG_DIR / QODERCN_CONFIG_DIR。
        修改模型、启用状态或路由端口后请再次同步，并新建会话；使用期间保持 Q
        Switch 运行。工具与 MCP 由 Qoder 管理。当前 Anthropic
        桥接在原生会话缺少稳定会话 ID 时会禁用工具；工具任务请先使用 Chat
        Completions 或 Responses 上游。
      </p>
    </div>
  );
}
