import { invoke } from "@tauri-apps/api/core";
export interface GatewayStatus {
  running: boolean;
  address: string;
  port: number;
  total_requests: number;
}
export const proxyApi = {
  getProxyStatus: () => invoke<GatewayStatus>("get_proxy_status"),
  startProxyServer: () => invoke<GatewayStatus>("start_proxy_server"),
  stopProxyWithRestore: () => invoke<void>("stop_proxy_with_restore"),
};
