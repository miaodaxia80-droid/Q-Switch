import { invoke } from "@tauri-apps/api/core";
export interface QoderRouteOption {
  routeId: string;
  displayName: string;
  providerId: string;
  model: string;
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
  syncNativeProvider: (target: "qoder" | "qoder-cn", routeId: string) =>
    invoke<string>("qoder_sync_native_provider", { target, routeId }),
  removeNativeProvider: (target: "qoder" | "qoder-cn") =>
    invoke<string>("qoder_remove_native_provider", { target }),
  listRoutes: () => invoke<QoderRouteOption[]>("qoder_list_routes"),
  listCustomProviders: () =>
    invoke<QoderCustomProvider[]>("qoder_list_custom_providers"),
  listCustomRoutes: () =>
    invoke<QoderCustomRoute[]>("qoder_list_custom_routes"),
  saveCustomProvider: (provider: QoderCustomProvider) =>
    invoke<QoderCustomProvider>("qoder_save_custom_provider", { provider }),
  saveCustomRoute: (route: QoderCustomRoute) =>
    invoke<QoderCustomRoute>("qoder_save_custom_route", { route }),
  deleteCustomProvider: (id: string) =>
    invoke<void>("qoder_delete_custom_provider", { id }),
  deleteCustomRoute: (id: string) =>
    invoke<void>("qoder_delete_custom_route", { id }),
};
