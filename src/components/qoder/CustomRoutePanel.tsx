import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
  Brain,
  Globe,
  KeyRound,
  Plus,
  RefreshCw,
  Server,
  Trash2,
} from "lucide-react";
import {
  qoderRouteApi,
  QODER_API_FORMAT_LABELS,
  QODER_API_FORMAT_OPTIONS,
  type QoderApiFormat,
  type QoderCustomProvider,
  type QoderCustomRoute,
} from "@/lib/api/qoderRoute";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

// NOTE: Radix Select rejects an empty-string item value, so "follow default"
// uses the explicit `default` sentinel rather than `value=""`.
type EffortValue = "default" | "low" | "medium" | "high";
type AddressMode = "base" | "full";

interface ModelForm {
  id: string;
  name: string;
  model: string;
  reasoningEffort: EffortValue;
  isReasoning: boolean;
  maxInputTokens: number;
}

interface ProviderForm {
  id: string;
  name: string;
  baseUrl: string;
  apiKey: string;
  clearKey: boolean;
  apiFormat: QoderApiFormat;
  addressMode: AddressMode;
  anthropicVersion: string;
  models: ModelForm[];
}

function emptyModel(): ModelForm {
  return {
    id: "",
    name: "",
    model: "",
    reasoningEffort: "default",
    isReasoning: true,
    maxInputTokens: 128_000,
  };
}

function emptyProviderForm(): ProviderForm {
  return {
    id: "",
    name: "",
    baseUrl: "",
    apiKey: "",
    clearKey: false,
    apiFormat: "openai_chat",
    addressMode: "base",
    anthropicVersion: "",
    models: [emptyModel()],
  };
}

function providerToForm(provider: QoderCustomProvider, models: QoderCustomRoute[]): ProviderForm {
  return {
    id: provider.id,
    name: provider.name,
    baseUrl: provider.baseUrl,
    apiKey: "",
    clearKey: false,
    apiFormat: provider.apiFormat,
    addressMode: provider.isFullUrl ? "full" : "base",
    anthropicVersion: provider.anthropicVersion ?? "",
    models: models.length
      ? models.map((model) => ({
          id: model.id,
          name: model.name,
          model: model.model,
          reasoningEffort: (model.reasoningEffort ?? "default") as EffortValue,
          isReasoning: model.isReasoning ?? true,
          maxInputTokens: model.maxInputTokens ?? 128_000,
        }))
      : [emptyModel()],
  };
}

/**
 * Qoder custom providers + models.
 *
 * A provider owns the shared connection (Base URL / full endpoint, API key,
 * API format) and one or more models. Qoder always speaks Chat to the local
 * QSwitch gateway; the gateway converts to the selected upstream protocol.
 */
export function CustomRoutePanel() {
  const [providers, setProviders] = useState<QoderCustomProvider[]>([]);
  const [models, setModels] = useState<QoderCustomRoute[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [modalOpen, setModalOpen] = useState(false);
  const [form, setForm] = useState<ProviderForm>(emptyProviderForm());
  const inFlight = useRef(false);

  const refresh = useCallback(async () => {
    if (inFlight.current) return;
    inFlight.current = true;
    try {
      const [providerList, routeList] = await Promise.all([
        qoderRouteApi.listCustomProviders(),
        qoderRouteApi.listCustomRoutes(),
      ]);
      setProviders(providerList);
      setModels(routeList);
    } catch (error) {
      console.error("[CustomRoutePanel] Failed to list custom providers:", error);
    } finally {
      inFlight.current = false;
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const interval = setInterval(() => void refresh(), 5000);
    return () => clearInterval(interval);
  }, [refresh]);

  const openCreate = () => {
    setForm(emptyProviderForm());
    setModalOpen(true);
  };

  const openEdit = (provider: QoderCustomProvider) => {
    const owned = models.filter((model) => model.providerId === provider.id);
    setForm(providerToForm(provider, owned));
    setModalOpen(true);
  };

  const patchForm = (patch: Partial<ProviderForm>) => setForm((prev) => ({ ...prev, ...patch }));

  const patchModel = (index: number, patch: Partial<ModelForm>) =>
    setForm((prev) => ({
      ...prev,
      models: prev.models.map((model, i) => (i === index ? { ...model, ...patch } : model)),
    }));

  const addModelRow = () =>
    setForm((prev) => ({ ...prev, models: [...prev.models, emptyModel()] }));

  const removeModelRow = (index: number) =>
    setForm((prev) => ({
      ...prev,
      models: prev.models.length > 1 ? prev.models.filter((_, i) => i !== index) : prev.models,
    }));

  const validate = (): string | null => {
    if (!form.name.trim()) return "请填写供应商名称。";
    if (!form.baseUrl.trim()) return "请填写 Base URL 或完整请求地址。";
    if (!/^https?:\/\//.test(form.baseUrl.trim()))
      return "请求地址必须以 http:// 或 https:// 开头。";
    const rows = form.models.filter((row) => row.name.trim() || row.model.trim());
    if (rows.length === 0) return "请至少添加一个模型。";
    for (const row of rows) {
      if (!row.name.trim() || !row.model.trim())
        return "每个模型都需要填写显示名称与上游模型名。";
    }
    return null;
  };

  const handleSave = async () => {
    const error = validate();
    if (error) {
      toast.error(error);
      return;
    }
    setSaving(true);
    try {
      const isFullUrl = form.addressMode === "full";
      // Empty input keeps the stored key; explicit clear sends "".
      const apiKey = form.clearKey ? "" : form.apiKey.trim() || null;
      const saved = await qoderRouteApi.saveCustomProvider({
        id: form.id,
        name: form.name.trim(),
        baseUrl: form.baseUrl.trim(),
        apiKey,
        apiFormat: form.apiFormat,
        isFullUrl,
        anthropicVersion: form.anthropicVersion.trim() || null,
        enabled: true,
      });

      const providerId = saved.id || form.id;
      const before = models.filter((model) => model.providerId === providerId);
      const keepIds = new Set(form.models.map((row) => row.id).filter(Boolean));
      const rows = form.models.filter((row) => row.name.trim() && row.model.trim());

      for (const row of rows) {
        await qoderRouteApi.saveCustomRoute({
          id: row.id,
          providerId,
          name: row.name.trim(),
          model: row.model.trim(),
          reasoningEffort: row.reasoningEffort === "default" ? null : row.reasoningEffort,
          isReasoning: row.isReasoning,
          maxInputTokens: Number(row.maxInputTokens) || 128_000,
          enabled: true,
        });
      }
      for (const old of before) {
        if (!keepIds.has(old.id)) await qoderRouteApi.deleteCustomRoute(old.id);
      }

      await qoderRouteApi.syncModelManifest();
      toast.success(`供应商「${saved.name}」已保存，Qoder 模型清单已刷新。`);
      setModalOpen(false);
      await refresh();
    } catch (err) {
      toast.error(`保存供应商失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const handleDeleteProvider = async (provider: QoderCustomProvider) => {
    const count = models.filter((m) => m.providerId === provider.id).length;
    if (
      !window.confirm(
        `删除供应商「${provider.name}」及其 ${count} 个模型？此操作不可撤销。`,
      )
    )
      return;
    try {
      await qoderRouteApi.deleteCustomProvider(provider.id);
      await qoderRouteApi.syncModelManifest();
      toast.success("供应商已删除。");
      await refresh();
    } catch (err) {
      toast.error(`删除供应商失败：${String(err)}`);
    }
  };

  const handleDeleteModel = async (model: QoderCustomRoute) => {
    if (!window.confirm(`删除模型「${model.name}」？`)) return;
    try {
      await qoderRouteApi.deleteCustomRoute(model.id);
      await qoderRouteApi.syncModelManifest();
      toast.success("模型已删除。");
      await refresh();
    } catch (err) {
      toast.error(`删除模型失败：${String(err)}`);
    }
  };

  return (
    <div className="space-y-3 rounded-lg border border-border-default bg-muted/30 p-3">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2 text-sm font-medium text-foreground">
          <Globe className="h-4 w-4 text-muted-foreground" />
          自定义模型供应商（任意请求地址 · 三种消息协议）
        </div>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="sm"
            className="h-7 px-2"
            onClick={() => void refresh()}
            disabled={loading}
          >
            <RefreshCw className={loading ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />
          </Button>
          <Button size="sm" className="h-7" onClick={openCreate}>
            <Plus className="mr-1 h-3.5 w-3.5" />
            添加模型供应商
          </Button>
        </div>
      </div>

      {providers.length === 0 ? (
        <p className="text-[11px] leading-relaxed text-muted-foreground">
          先添加供应商（Base URL / 完整地址、API Key、API 格式），再在其下添加一个或多个模型。
          Qoder 只对接 QSwitch 本地 Chat 入口，由 QSwitch 转换为 Chat Completions、
          Anthropic Messages 或 Responses，无需改动 Qoder 安装包。
        </p>
      ) : (
        <div className="space-y-2">
          {providers.map((provider) => {
            const owned = models.filter((m) => m.providerId === provider.id);
            return (
              <div
                key={provider.id}
                className="rounded-md border border-border-default bg-background/60 px-2.5 py-2"
              >
                <div className="flex items-center justify-between gap-2">
                  <div className="flex min-w-0 items-center gap-2">
                    <Server className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                    <p className="truncate text-[13px] font-medium text-foreground">
                      {provider.name}
                    </p>
                    <span className="shrink-0 rounded-full bg-violet-500/10 px-2 py-0.5 text-[10px] text-violet-600 dark:text-violet-300">
                      {QODER_API_FORMAT_LABELS[provider.apiFormat] ?? provider.apiFormat}
                    </span>
                    <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-[10px] text-muted-foreground">
                      {provider.isFullUrl ? "完整地址" : "Base URL"}
                    </span>
                  </div>
                  <div className="flex shrink-0 items-center gap-1">
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-7 px-2 text-xs"
                      onClick={() => openEdit(provider)}
                    >
                      编辑
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-7 w-7 text-destructive"
                      onClick={() => void handleDeleteProvider(provider)}
                      title="删除供应商"
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                </div>
                <p className="mt-0.5 truncate pl-5 text-[11px] text-muted-foreground">
                  {provider.baseUrl} · {provider.hasApiKey ? "已配置 API Key" : "无 API Key"}
                </p>
                <div className="mt-1.5 space-y-1 pl-5">
                  {owned.map((model) => (
                    <div
                      key={model.id}
                      className="flex items-center justify-between gap-2 rounded bg-muted/40 px-2 py-1"
                    >
                      <div className="flex min-w-0 items-center gap-2">
                        <span className="truncate text-[12px] text-foreground">{model.name}</span>
                        <span className="shrink-0 text-[10px] text-muted-foreground">
                          {model.model}
                        </span>
                        {model.reasoningEffort && (
                          <span className="inline-flex shrink-0 items-center gap-0.5 rounded-full bg-violet-500/10 px-1.5 py-0.5 text-[10px] text-violet-600 dark:text-violet-300">
                            <Brain className="h-2.5 w-2.5" />
                            {model.reasoningEffort}
                          </span>
                        )}
                      </div>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-6 w-6 shrink-0 text-destructive"
                        onClick={() => void handleDeleteModel(model)}
                        title="删除模型"
                      >
                        <Trash2 className="h-3 w-3" />
                      </Button>
                    </div>
                  ))}
                  {owned.length === 0 && (
                    <p className="text-[10px] text-muted-foreground">该供应商下还没有模型。</p>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      )}

      <Dialog open={modalOpen} onOpenChange={setModalOpen}>
        <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>{form.id ? "编辑模型供应商" : "添加模型供应商"}</DialogTitle>
            <DialogDescription>
              供应商定义连接与协议，其下可添加多个模型。保存后自动刷新 Qoder 模型清单。
            </DialogDescription>
          </DialogHeader>

          <div className="grid gap-3 py-2">
            <div className="grid gap-1.5">
              <Label htmlFor="cp-name">供应商名称</Label>
              <Input
                id="cp-name"
                value={form.name}
                onChange={(e) => patchForm({ name: e.target.value })}
                placeholder="例如：我的 Anthropic 中转"
              />
            </div>

            <div className="grid grid-cols-2 gap-2">
              <div className="grid gap-1.5">
                <Label>API 格式</Label>
                <Select
                  value={form.apiFormat}
                  onValueChange={(value) => patchForm({ apiFormat: value as QoderApiFormat })}
                >
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {QODER_API_FORMAT_OPTIONS.map((fmt) => (
                      <SelectItem key={fmt} value={fmt}>
                        {QODER_API_FORMAT_LABELS[fmt]}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div className="grid gap-1.5">
                <Label>地址类型</Label>
                <Select
                  value={form.addressMode}
                  onValueChange={(value) => patchForm({ addressMode: value as AddressMode })}
                >
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="base">Base URL（自动补路径）</SelectItem>
                    <SelectItem value="full">完整请求地址</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>

            <div className="grid gap-1.5">
              <Label htmlFor="cp-url">
                {form.addressMode === "full" ? "完整请求地址（不追加路径）" : "Base URL"}
              </Label>
              <Input
                id="cp-url"
                value={form.baseUrl}
                onChange={(e) => patchForm({ baseUrl: e.target.value })}
                placeholder={
                  form.apiFormat === "anthropic_messages"
                    ? "https://api.anthropic.com"
                    : form.addressMode === "full"
                      ? "https://gateway.example.com/custom/chat"
                      : "https://api.example.com/v1"
                }
              />
              <p className="text-[10px] text-muted-foreground">
                {form.addressMode === "base"
                  ? `将自动追加：${
                      form.apiFormat === "anthropic_messages"
                        ? "/v1/messages"
                        : form.apiFormat === "openai_responses"
                          ? "/responses"
                          : "/chat/completions"
                    }（显式 query 参数会保留）`
                  : "完整地址模式直接请求该 URL，不再追加任何路径。"}
              </p>
            </div>

            <div className="grid gap-1.5">
              <Label htmlFor="cp-key" className="flex items-center gap-1.5">
                <KeyRound className="h-3 w-3 text-muted-foreground" />
                API Key{form.id ? "（留空保持不变）" : "（可选）"}
              </Label>
              <Input
                id="cp-key"
                type="password"
                value={form.apiKey}
                onChange={(e) => patchForm({ apiKey: e.target.value, clearKey: false })}
                placeholder={form.id ? "已保存，留空不修改" : "sk-..."}
                autoComplete="off"
              />
              {form.id && (
                <label className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
                  <Switch
                    checked={form.clearKey}
                    onCheckedChange={(checked) => patchForm({ clearKey: checked, apiKey: "" })}
                  />
                  清除已保存的 API Key
                </label>
              )}
            </div>

            {form.apiFormat === "anthropic_messages" && (
              <div className="grid gap-1.5">
                <Label htmlFor="cp-ver">anthropic-version（可选）</Label>
                <Input
                  id="cp-ver"
                  value={form.anthropicVersion}
                  onChange={(e) => patchForm({ anthropicVersion: e.target.value })}
                  placeholder="2023-06-01"
                />
              </div>
            )}

            <div className="mt-1 space-y-2 rounded-md border border-border-default p-2">
              <div className="flex items-center justify-between">
                <span className="text-xs font-medium text-foreground">模型列表</span>
                <Button variant="outline" size="sm" className="h-7" onClick={addModelRow}>
                  <Plus className="mr-1 h-3 w-3" />
                  添加模型
                </Button>
              </div>
              {form.models.map((model, index) => (
                <div key={index} className="grid gap-2 rounded bg-muted/40 p-2">
                  <div className="flex items-center gap-2">
                    <Input
                      value={model.name}
                      onChange={(e) => patchModel(index, { name: e.target.value })}
                      placeholder="显示名称，例如 Claude Sonnet"
                      className="h-8 text-xs"
                    />
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-8 w-8 shrink-0 text-destructive"
                      onClick={() => removeModelRow(index)}
                      title="移除该模型"
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                  <Input
                    value={model.model}
                    onChange={(e) => patchModel(index, { model: e.target.value })}
                    placeholder="上游模型名，例如 claude-sonnet-4-5"
                    className="h-8 text-xs"
                  />
                  <div className="grid grid-cols-2 gap-2">
                    <Select
                      value={model.reasoningEffort}
                      onValueChange={(value) =>
                        patchModel(index, { reasoningEffort: value as EffortValue })
                      }
                    >
                      <SelectTrigger className="h-8 text-xs">
                        <SelectValue placeholder="思考强度" />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="default">跟随模型默认</SelectItem>
                        <SelectItem value="low">低（low）</SelectItem>
                        <SelectItem value="medium">中（medium）</SelectItem>
                        <SelectItem value="high">高（high）</SelectItem>
                      </SelectContent>
                    </Select>
                    <Input
                      type="number"
                      value={model.maxInputTokens}
                      onChange={(e) =>
                        patchModel(index, { maxInputTokens: Number(e.target.value) || 128_000 })
                      }
                      className="h-8 text-xs"
                      placeholder="max_input_tokens"
                    />
                  </div>
                  <label className="flex items-center gap-2 text-[11px] text-muted-foreground">
                    <Switch
                      checked={model.isReasoning}
                      onCheckedChange={(checked) => patchModel(index, { isReasoning: checked })}
                    />
                    支持推理（reasoning）
                  </label>
                </div>
              ))}
            </div>
          </div>

          <DialogFooter>
            <Button variant="outline" onClick={() => setModalOpen(false)}>
              取消
            </Button>
            <Button onClick={() => void handleSave()} disabled={saving}>
              {saving ? "保存中…" : "保存供应商"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
