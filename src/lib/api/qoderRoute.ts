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
    await invoke("qoder_remove_carrier_mapping", { carrierModelId });
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
};
