import { invoke } from "@tauri-apps/api/core";

export interface QoderNativeAdapterStatus {
  active: boolean;
  adapterPort: number | null;
  adapterIpcPath: string | null;
  nativePort: number | null;
  nativePid: number | null;
  observedCustomModelIds: string[];
  clientConnected: boolean;
  activeClientConnections: number;
}

export interface QoderRouteOption {
  routeId: string;
  displayName: string;
  providerId: string;
  model: string;
}

export interface QoderCarrierMapping {
  carrierModelId: string;
  routeId: string;
}

export interface QoderToolPolicy {
  allowTerminal: boolean;
  allowWrite: boolean;
  allowNetwork: boolean;
  allowPrivateNetwork: boolean;
  allowMcp: boolean;
}

/// Upstream wire formats supported by the Qoder bridge.
export type QoderApiFormat =
  | "openai_chat"
  | "anthropic_messages"
  | "openai_responses";

export const QODER_API_FORMAT_LABELS: Record<QoderApiFormat, string> = {
  openai_chat: "Chat Completions (/chat/completions)",
  anthropic_messages: "Anthropic Messages (/v1/messages)",
  openai_responses: "Responses (/responses)",
};

export const QODER_API_FORMAT_OPTIONS: QoderApiFormat[] = [
  "openai_chat",
  "anthropic_messages",
  "openai_responses",
];

/** A shared upstream connection owning one or more models. */
export interface QoderCustomProvider {
  id: string;
  name: string;
  baseUrl: string;
  /** Sent on save; never returned by list/save (use hasApiKey). */
  apiKey?: string | null;
  hasApiKey?: boolean;
  apiFormat: QoderApiFormat;
  isFullUrl: boolean;
  anthropicVersion?: string | null;
  enabled: boolean;
}

/** A model owned by a QoderCustomProvider. */
export interface QoderCustomRoute {
  id: string;
  providerId: string;
  name: string;
  model: string;
  reasoningEffort?: string | null;
  isReasoning?: boolean;
  maxInputTokens?: number;
  enabled?: boolean;
}

export const qoderRouteApi = {
  async syncModelManifest(): Promise<string> {
    return await invoke("qoder_sync_model_manifest");
  },

  async listRoutes(): Promise<QoderRouteOption[]> {
    return await invoke("qoder_list_routes");
  },

  async listCarrierMappings(): Promise<QoderCarrierMapping[]> {
    return await invoke("qoder_list_carrier_mappings");
  },

  async saveCarrierMapping(
    carrierModelId: string,
    routeId: string,
  ): Promise<QoderCarrierMapping> {
    return await invoke("qoder_save_carrier_mapping", {
      carrierModelId,
      routeId,
    });
  },

  async removeCarrierMapping(carrierModelId: string): Promise<void> {
    return await invoke("qoder_remove_carrier_mapping", { carrierModelId });
  },

  async getToolPolicy(): Promise<QoderToolPolicy> {
    return await invoke("qoder_get_tool_policy");
  },

  async setToolPolicy(policy: QoderToolPolicy): Promise<QoderToolPolicy> {
    return await invoke("qoder_set_tool_policy", { policy });
  },

  async getNativeAdapterStatus(): Promise<QoderNativeAdapterStatus> {
    return await invoke("qoder_get_native_adapter_status");
  },

  async startNativeAdapter(): Promise<QoderNativeAdapterStatus> {
    return await invoke("qoder_start_native_adapter");
  },

  async stopNativeAdapter(): Promise<QoderNativeAdapterStatus> {
    return await invoke("qoder_stop_native_adapter");
  },

  async listCustomRoutes(): Promise<QoderCustomRoute[]> {
    return await invoke("qoder_list_custom_routes");
  },

  async saveCustomRoute(route: QoderCustomRoute): Promise<QoderCustomRoute> {
    return await invoke("qoder_save_custom_route", { route });
  },

  async deleteCustomRoute(id: string): Promise<void> {
    return await invoke("qoder_delete_custom_route", { id });
  },

  async listCustomProviders(): Promise<QoderCustomProvider[]> {
    return await invoke("qoder_list_custom_providers");
  },

  async saveCustomProvider(
    provider: QoderCustomProvider,
  ): Promise<QoderCustomProvider> {
    return await invoke("qoder_save_custom_provider", { provider });
  },

  async deleteCustomProvider(id: string): Promise<void> {
    return await invoke("qoder_delete_custom_provider", { id });
  },
};
