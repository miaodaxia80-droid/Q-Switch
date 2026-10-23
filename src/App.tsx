import { useCallback, useEffect, useState } from "react";
import { Cable, RefreshCw, Power, CircleHelp } from "lucide-react";
import { toast } from "sonner";
import { CustomRoutePanel } from "@/components/qoder/CustomRoutePanel";
import { NativeProviderPanel } from "@/components/qoder/NativeProviderPanel";
import { qoderRouteApi, type QoderRouteOption } from "@/lib/api/qoderRoute";
import { proxyApi, type GatewayStatus } from "@/lib/api/proxy";
import { Button } from "@/components/ui/button";

export default function App() {
  const [status, setStatus] = useState<GatewayStatus | null>(null);
  const [routes, setRoutes] = useState<QoderRouteOption[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [help, setHelp] = useState(false);
  const refresh = useCallback(async () => {
    try {
      const [next, models] = await Promise.all([
        proxyApi.getProxyStatus(),
        qoderRouteApi.listRoutes(),
      ]);
      setStatus(next);
      setRoutes(models);
      setError("");
    } catch (error) {
      setError(String(error));
    }
  }, []);
  useEffect(() => {
    void refresh();
    const onFocus = () => void refresh();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refresh]);
  const toggle = async () => {
    setBusy(true);
    try {
      if (status?.running) await proxyApi.stopProxyWithRestore();
      else await proxyApi.startProxyServer();
      await refresh();
    } catch (error) {
      setError(String(error));
      toast.error(String(error));
    } finally {
      setBusy(false);
    }
  };
  return (
    <main className="mx-auto min-h-screen max-w-3xl space-y-6 px-6 pb-8 pt-12">
      <header className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <span className="rounded-xl bg-primary/10 p-2.5 text-primary">
            <Cable className="size-6" />
          </span>
          <div>
            <h1 className="text-xl font-semibold tracking-tight">Q Switch</h1>
            <p className="text-xs text-muted-foreground">
              你的 Qoder，自选模型。
            </p>
          </div>
        </div>
        <Button
          variant="ghost"
          size="icon"
          aria-label="使用说明"
          onClick={() => setHelp(!help)}
        >
          <CircleHelp className="size-4" />
        </Button>
      </header>
      <section className="flex flex-wrap items-center justify-between gap-3 rounded-xl border bg-card p-4">
        <div className="space-y-1">
          <p className="flex items-center gap-2 text-sm font-medium">
            <span
              className={`size-2 rounded-full ${status?.running ? "bg-emerald-500" : "bg-muted-foreground"}`}
            />
            {status?.running ? "本地路由运行中" : "本地路由已停止"}
          </p>
          <p className="font-mono text-xs text-muted-foreground">
            127.0.0.1:{status?.port ?? 15731} · {routes.length} 个模型
          </p>
        </div>
        <div className="flex gap-2">
          <Button
            size="icon"
            variant="ghost"
            aria-label="刷新状态"
            onClick={() => void refresh()}
          >
            <RefreshCw className="size-4" />
          </Button>
          <Button
            size="sm"
            variant={status?.running ? "outline" : "default"}
            disabled={busy || status === null}
            onClick={() => void toggle()}
          >
            <Power className="mr-2 size-3.5" />
            {status?.running ? "停止" : "启动"}
          </Button>
        </div>
      </section>
      {error && (
        <p
          role="alert"
          className="rounded-lg bg-destructive/10 p-3 text-sm text-destructive"
        >
          {error}
        </p>
      )}
      <CustomRoutePanel onChanged={refresh} />
      <NativeProviderPanel
        routes={routes}
        isRunning={status?.running ?? false}
      />
      {help && (
        <section className="space-y-2 rounded-xl border p-4 text-xs leading-6 text-muted-foreground">
          <p>1. 添加供应商，填写接口、密钥和模型名。</p>
          <p>2. 选择国际版或 CN 版，点击「同步模型并选择」。</p>
          <p>3. 在 Qoder 新建会话，保持 Q Switch 运行。</p>
          <p>
            关闭窗口后路由继续运行；从托盘菜单退出应用。原生工具与 MCP 由 Qoder
            管理。
          </p>
          <p>
            需要支持原生 providers 配置的 Qoder CLI / App 和 BYOK 账号权限。
          </p>
        </section>
      )}
      <footer className="text-center text-[11px] text-muted-foreground">
        本地配置 · 按需转发 · 工具由 Qoder 执行
      </footer>
    </main>
  );
}
