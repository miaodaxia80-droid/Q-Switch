import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  Cable,
  Link2,
  Plus,
  RefreshCw,
  Route,
  ShieldAlert,
  Trash2,
} from "lucide-react";
import {
  qoderRouteApi,
  type QoderCarrierMapping,
  type QoderNativeAdapterStatus,
  type QoderRouteOption,
  type QoderToolPolicy,
} from "@/lib/api/qoderRoute";
import { proxyApi } from "@/lib/api/proxy";
import type { ProxyStatus } from "@/types/proxy";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import { Switch } from "@/components/ui/switch";

/**
 * Qoder native BYOK + local routing status.
 *
 */
export function QoderRoutePanel() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [adapterStatus, setAdapterStatus] =
    useState<QoderNativeAdapterStatus | null>(null);
  const [routes, setRoutes] = useState<QoderRouteOption[]>([]);
  const [carrierMappings, setCarrierMappings] = useState<QoderCarrierMapping[]>(
    [],
  );
  const [toolPolicy, setToolPolicy] = useState<QoderToolPolicy | null>(null);
  const [carrierModelId, setCarrierModelId] = useState("");
  const [routeId, setRouteId] = useState("");
  const [loading, setLoading] = useState(false);
  const [adapterLoading, setAdapterLoading] = useState(false);
  const [mappingLoading, setMappingLoading] = useState(false);
  const [policyLoading, setPolicyLoading] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [
        proxyStatus,
        nativeAdapterStatus,
        availableRoutes,
        mappings,
        policy,
      ] = await Promise.all([
        proxyApi.getProxyStatus(),
        qoderRouteApi.getNativeAdapterStatus(),
        qoderRouteApi.listRoutes(),
        qoderRouteApi.listCarrierMappings(),
        qoderRouteApi.getToolPolicy(),
      ]);
      setStatus(proxyStatus);
      setAdapterStatus(nativeAdapterStatus);
      setRoutes(availableRoutes);
      setCarrierMappings(mappings);
      setToolPolicy(policy);
      setRouteId((current) =>
        availableRoutes.some((route) => route.routeId === current)
          ? current
          : availableRoutes[0]?.routeId || "",
      );
    } catch (error) {
      console.error("[QoderRoutePanel] Failed to get gateway status:", error);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const interval = setInterval(() => void refresh(), 5000);
    return () => clearInterval(interval);
  }, [refresh]);

  const handleSyncManifest = async () => {
    setLoading(true);
    try {
      const path = await qoderRouteApi.syncModelManifest();
      toast.success(
        t("qoder.models.synced", {
          defaultValue: "Qoder 自定义模型清单已刷新：{{path}}",
          path,
        }),
      );
      await refresh();
    } catch (error) {
      toast.error(
        t("qoder.models.syncFailed", {
          defaultValue: "刷新 Qoder 模型清单失败：{{error}}",
          error: String(error),
        }),
      );
    } finally {
      setLoading(false);
    }
  };

  const handleSaveCarrierMapping = async () => {
    if (!carrierModelId.trim() || !routeId) {
      toast.error("请填写 Qoder 载体模型 ID 并选择 Q Switch 路由。");
      return;
    }
    setMappingLoading(true);
    try {
      await qoderRouteApi.saveCarrierMapping(carrierModelId, routeId);
      setCarrierModelId("");
      toast.success("Qoder 载体模型映射已保存。");
      await refresh();
    } catch (error) {
      toast.error(`保存 Qoder 载体模型映射失败：${String(error)}`);
    } finally {
      setMappingLoading(false);
    }
  };

  const handleRemoveCarrierMapping = async (mapping: QoderCarrierMapping) => {
    setMappingLoading(true);
    try {
      await qoderRouteApi.removeCarrierMapping(mapping.carrierModelId);
      toast.success("Qoder 载体模型映射已移除。");
      await refresh();
    } catch (error) {
      toast.error(`移除 Qoder 载体模型映射失败：${String(error)}`);
    } finally {
      setMappingLoading(false);
    }
  };

  const handleToggleNativeAdapter = async () => {
    setAdapterLoading(true);
    try {
      const next = adapterStatus?.active
        ? await qoderRouteApi.stopNativeAdapter()
        : await qoderRouteApi.startNativeAdapter();
      setAdapterStatus(next);
      toast.success(
        next.active
          ? "Qoder 原生传输适配器已启用。先在 Qoder 选择载体模型；未映射 ID 会显示在此面板，保存映射后重新选择它。若刚重启 Qoder，请等待原生 Agent 就绪后再启用。"
          : "Qoder 原生传输适配器已关闭，原生 Agent 端点已恢复。",
      );
      await refresh();
    } catch (error) {
      toast.error(`Qoder 原生传输适配器操作失败：${String(error)}`);
    } finally {
      setAdapterLoading(false);
    }
  };

  const handleToolPolicyChange = async (changes: Partial<QoderToolPolicy>) => {
    if (!toolPolicy) return;
    setPolicyLoading(true);
    try {
      const updated = await qoderRouteApi.setToolPolicy({
        ...toolPolicy,
        ...changes,
      });
      setToolPolicy(updated);
      toast.success("Qoder 自定义模型权限已更新；下一个请求立即生效。");
    } catch (error) {
      toast.error(`更新 Qoder 自定义模型权限失败：${String(error)}`);
    } finally {
      setPolicyLoading(false);
    }
  };

  const isRunning = status?.running ?? false;
  const endpoint = `http://${status?.address || "127.0.0.1"}:${status?.port || 15731}/qoder/v1`;
  const routeById = new Map(routes.map((route) => [route.routeId, route]));
  const observedCarrierModelIds = adapterStatus?.observedCustomModelIds ?? [];

  return (
    <div className="space-y-4 px-1">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <div
            className={cn(
              "flex items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium",
              isRunning
                ? "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
                : "bg-muted text-muted-foreground",
            )}
          >
            <span
              className={cn(
                "h-1.5 w-1.5 rounded-full",
                isRunning
                  ? "bg-emerald-500 animate-pulse"
                  : "bg-muted-foreground",
              )}
            />
            {isRunning
              ? t("qoder.route.running", {
                  defaultValue: "Q Switch 路由端已就绪",
                })
              : t("qoder.route.stopped", {
                  defaultValue: "Q Switch 路由端未启动",
                })}
          </div>
          <span className="flex items-center gap-1 text-xs text-muted-foreground">
            <Route className="h-3 w-3" />
            {endpoint}
          </span>
        </div>
        <Button
          variant="ghost"
          size="icon"
          onClick={() => void refresh()}
          className="h-7 w-7"
          title={t("common.refresh", { defaultValue: "刷新" })}
        >
          <RefreshCw className="h-3.5 w-3.5" />
        </Button>
      </div>

      <Button
        onClick={() => void handleSyncManifest()}
        disabled={loading}
        variant="outline"
        size="sm"
      >
        <RefreshCw className={cn("mr-2 h-4 w-4", loading && "animate-spin")} />
        {t("qoder.models.sync", {
          defaultValue: "刷新 Q Switch 路由清单",
        })}
      </Button>

      <div className="space-y-3 rounded-lg border border-border-default bg-muted/30 p-3">
        <div className="flex items-center gap-2 text-sm font-medium text-foreground">
          <Link2 className="h-4 w-4 text-muted-foreground" />
          Qoder 载体模型映射
        </div>
        <div className="grid gap-2 md:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto]">
          <Input
            value={carrierModelId}
            onChange={(event) => setCarrierModelId(event.target.value)}
            placeholder="custom:model_..."
            aria-label="Qoder 载体模型 ID"
            disabled={mappingLoading}
          />
          <Select
            value={routeId}
            onValueChange={setRouteId}
            disabled={mappingLoading || routes.length === 0}
          >
            <SelectTrigger aria-label="Q Switch 路由">
              <SelectValue placeholder="选择 Q Switch 路由" />
            </SelectTrigger>
            <SelectContent>
              {routes.map((route) => (
                <SelectItem key={route.routeId} value={route.routeId}>
                  {route.displayName}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button
            onClick={() => void handleSaveCarrierMapping()}
            disabled={mappingLoading || routes.length === 0}
            size="sm"
            className="md:self-center"
          >
            <Plus className="mr-2 h-4 w-4" />
            保存映射
          </Button>
        </div>
        <p className="text-[11px] text-muted-foreground">
          先在 Qoder 原生 BYOK 中选择一次载体模型。适配器会显示实际发出的
          custom: 模型 ID；只有已保存的映射会被本机适配器接管。
        </p>
        {adapterStatus?.active && (
          <div className="rounded-md border border-dashed border-border-default bg-background/60 p-2">
            {!adapterStatus.clientConnected ? (
              <div className="space-y-1">
                <p className="text-[11px] font-medium text-amber-600 dark:text-amber-400">
                  Qoder 尚未通过适配器连接，当前无法观察载体模型选择。
                </p>
                <p className="text-[11px] text-muted-foreground">
                  Qoder 会保持启用适配器之前建立的原生连接，只有重新连接后才会走
                  适配器。请重启 Qoder（或新开一个 Quest
                  窗口），再次选择一次载体模型，然后回到这里刷新。
                </p>
              </div>
            ) : observedCarrierModelIds.length > 0 ? (
              <>
                <p className="mb-2 text-[11px] text-muted-foreground">
                  已检测到未映射的 Qoder 载体模型。点击即可填入上方输入框：
                </p>
                <div className="flex flex-wrap gap-2">
                  {observedCarrierModelIds.map((modelId) => (
                    <Button
                      key={modelId}
                      variant="outline"
                      size="sm"
                      className="h-7 max-w-full font-mono text-[11px]"
                      onClick={() => setCarrierModelId(`custom:${modelId}`)}
                      disabled={mappingLoading}
                    >
                      <span className="truncate">custom:{modelId}</span>
                    </Button>
                  ))}
                </div>
              </>
            ) : (
              <p className="text-[11px] text-muted-foreground">
                尚未检测到载体模型。请在 Qoder 重新选择一次原生 BYOK
                模型，再回到这里刷新。
              </p>
            )}
          </div>
        )}
        {carrierMappings.length > 0 && (
          <div className="divide-y divide-border-default rounded-md border border-border-default bg-background">
            {carrierMappings.map((mapping) => {
              const route = routeById.get(mapping.routeId);
              return (
                <div
                  key={mapping.carrierModelId}
                  className="flex min-w-0 items-center gap-2 px-3 py-2 text-xs"
                >
                  <code className="min-w-0 flex-1 truncate text-foreground">
                    custom:{mapping.carrierModelId}
                  </code>
                  <span className="shrink-0 text-muted-foreground">→</span>
                  <span className="min-w-0 flex-1 truncate text-muted-foreground">
                    {route?.displayName || mapping.routeId}
                  </span>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="h-7 w-7 shrink-0"
                    title="移除映射"
                    aria-label={`移除 ${mapping.carrierModelId} 的映射`}
                    onClick={() => void handleRemoveCarrierMapping(mapping)}
                    disabled={mappingLoading}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </Button>
                </div>
              );
            })}
          </div>
        )}
      </div>

      <div className="rounded-lg border border-border-default bg-muted/30 p-3">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-2">
            <Cable
              className={cn(
                "mt-0.5 h-4 w-4 flex-shrink-0",
                adapterStatus?.active
                  ? "text-emerald-500"
                  : "text-muted-foreground",
              )}
            />
            <div className="space-y-1 text-xs text-muted-foreground">
              <p className="font-medium text-foreground">
                {adapterStatus?.active
                  ? "Qoder 原生传输适配器已启用"
                  : "Qoder 原生传输适配器未启用"}
              </p>
              <p>
                {adapterStatus?.active
                  ? `Electron → Q Switch IPC → 原生 Agent :${adapterStatus.nativePort}（兼容 WebSocket :${adapterStatus.adapterPort}）`
                  : "启用后仅接管 Q Switch 路由或已保存的载体映射；官方模型与未映射的 Qoder 原生 BYOK 均透明转发。"}
              </p>
              {adapterStatus?.active && (
                <p
                  className={cn(
                    "text-[11px]",
                    adapterStatus.clientConnected
                      ? "text-emerald-600 dark:text-emerald-400"
                      : "text-amber-600 dark:text-amber-400",
                  )}
                >
                  {adapterStatus.clientConnected
                    ? `Qoder 已通过适配器连接（${adapterStatus.activeClientConnections} 个活跃连接）`
                    : "Qoder 尚未通过适配器连接：请重启 Qoder 或新开 Quest 窗口后再选择载体模型"}
                </p>
              )}
            </div>
          </div>
          <Button
            onClick={() => void handleToggleNativeAdapter()}
            disabled={adapterLoading || (!adapterStatus?.active && !isRunning)}
            variant={adapterStatus?.active ? "outline" : "default"}
            size="sm"
          >
            <Cable
              className={cn("mr-2 h-4 w-4", adapterLoading && "animate-pulse")}
            />
            {adapterStatus?.active ? "关闭适配器" : "启用原生适配器"}
          </Button>
        </div>
        {!isRunning && !adapterStatus?.active && (
          <p className="mt-2 text-[11px] text-amber-600 dark:text-amber-400">
            请先启动 Q Switch 路由服务；适配器只会将 Qoder
            自定义模型转发到本机路由。
          </p>
        )}
      </div>

      <div className="space-y-3 rounded-lg border border-border-default bg-muted/30 p-3">
        <div className="flex items-start gap-2">
          <ShieldAlert className="mt-0.5 h-4 w-4 flex-shrink-0 text-amber-500" />
          <div className="space-y-1">
            <p className="text-sm font-medium text-foreground">
              Qoder 自定义模型权限
            </p>
            <p className="text-[11px] text-muted-foreground">
              默认只允许读取工作区。开启下列能力等同于允许当前路由的上游模型使用本机工具；切换在下一个
              Quest 请求生效。
            </p>
          </div>
        </div>

        <div className="divide-y divide-border-default rounded-md border border-border-default bg-background">
          <div className="flex items-center justify-between gap-3 px-3 py-2.5">
            <div className="min-w-0">
              <p className="text-xs font-medium text-foreground">
                写入工作区文件
              </p>
              <p className="text-[11px] text-muted-foreground">
                仅可创建或替换当前工作区内的 UTF-8 文件。
              </p>
            </div>
            <Switch
              checked={toolPolicy?.allowWrite ?? false}
              disabled={!toolPolicy || policyLoading}
              onCheckedChange={(allowWrite) =>
                void handleToolPolicyChange({ allowWrite })
              }
              aria-label="允许 Qoder 自定义模型写入工作区文件"
            />
          </div>
          <div className="flex items-center justify-between gap-3 px-3 py-2.5">
            <div className="min-w-0">
              <p className="text-xs font-medium text-foreground">终端</p>
              <p className="text-[11px] text-muted-foreground">
                命令以当前 macOS/Windows
                用户身份运行，默认工作目录为当前工作区；这不是系统级沙箱。
              </p>
            </div>
            <Switch
              checked={toolPolicy?.allowTerminal ?? false}
              disabled={!toolPolicy || policyLoading}
              onCheckedChange={(allowTerminal) =>
                void handleToolPolicyChange({ allowTerminal })
              }
              aria-label="允许 Qoder 自定义模型使用终端"
            />
          </div>
          <div className="flex items-center justify-between gap-3 px-3 py-2.5">
            <div className="min-w-0">
              <p className="text-xs font-medium text-foreground">网络请求</p>
              <p className="text-[11px] text-muted-foreground">
                允许受限的 HTTP/HTTPS 请求；默认阻止本机、私有和链路本地地址。
              </p>
            </div>
            <Switch
              checked={toolPolicy?.allowNetwork ?? false}
              disabled={!toolPolicy || policyLoading}
              onCheckedChange={(allowNetwork) =>
                void handleToolPolicyChange({
                  allowNetwork,
                  allowPrivateNetwork: allowNetwork
                    ? (toolPolicy?.allowPrivateNetwork ?? false)
                    : false,
                })
              }
              aria-label="允许 Qoder 自定义模型访问网络"
            />
          </div>
          <div className="flex items-center justify-between gap-3 px-3 py-2.5">
            <div className="min-w-0">
              <p className="text-xs font-medium text-foreground">私有网络</p>
              <p className="text-[11px] text-muted-foreground">
                允许访问
                localhost、局域网与内网服务；仅在你明确需要本地服务时开启。
              </p>
            </div>
            <Switch
              checked={toolPolicy?.allowPrivateNetwork ?? false}
              disabled={!toolPolicy?.allowNetwork || policyLoading}
              onCheckedChange={(allowPrivateNetwork) =>
                void handleToolPolicyChange({ allowPrivateNetwork })
              }
              aria-label="允许 Qoder 自定义模型访问私有网络"
            />
          </div>
          <div className="flex items-center justify-between gap-3 px-3 py-2.5">
            <div className="min-w-0">
              <p className="text-xs font-medium text-foreground">MCP 工具</p>
              <p className="text-[11px] text-muted-foreground">
                还需要在“工具 / MCP”中为对应服务器勾选 Qoder；适配器支持 stdio
                与 Streamable HTTP。
              </p>
            </div>
            <Switch
              checked={toolPolicy?.allowMcp ?? false}
              disabled={!toolPolicy || policyLoading}
              onCheckedChange={(allowMcp) =>
                void handleToolPolicyChange({ allowMcp })
              }
              aria-label="允许 Qoder 自定义模型使用 MCP"
            />
          </div>
        </div>
      </div>

      <div className="rounded-lg border border-border-default bg-muted/30 p-3">
        <div className="flex items-start gap-2">
          <ShieldAlert className="mt-0.5 h-4 w-4 flex-shrink-0 text-amber-500" />
          <div className="space-y-1 text-xs text-muted-foreground">
            <p>
              {t("qoder.route.description", {
                defaultValue: adapterStatus?.active
                  ? "适配器已接管 Qoder 的本机 WebSocket 入口，并会在原生 Agent 刷新时自动重挂；只有 Q Switch 路由或已保存的载体映射会进入 Q Switch。未映射载体的模型 ID 只在本机内存中显示，不会记录提示词或密钥。新建 Quest 后用路由日志确认首个真实请求。"
                  : "路由清单和转发端点已经可用，但 Qoder 原生 Agent 的传输适配器尚未接通。当前不能把“模型出现在列表里”当作请求已走 Q Switch。",
              })}
            </p>
            <p className="text-[11px]">
              {t("qoder.route.security", {
                defaultValue:
                  "真实 API Key 仍只保存在 Q Switch 中。适配器不会改写 Qoder 应用包，并且关闭时只恢复自己写入的本机发现记录。",
              })}
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}
