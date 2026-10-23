import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "@/App";
import { proxyApi } from "@/lib/api/proxy";
import { qoderRouteApi } from "@/lib/api/qoderRoute";

vi.mock("@/lib/api/proxy", () => ({
  proxyApi: {
    getProxyStatus: vi.fn(),
    startProxyServer: vi.fn(),
    stopProxyWithRestore: vi.fn(),
  },
}));
vi.mock("@/lib/api/qoderRoute", () => ({
  qoderRouteApi: { listRoutes: vi.fn() },
}));
vi.mock("@/components/qoder/CustomRoutePanel", () => ({
  CustomRoutePanel: () => <div>供应商模型配置</div>,
}));
vi.mock("@/components/qoder/NativeProviderPanel", () => ({
  NativeProviderPanel: () => <div>原生模型接入</div>,
}));
vi.mock("sonner", () => ({ toast: { error: vi.fn() } }));
const status = {
  running: false,
  address: "127.0.0.1",
  port: 15731,
  total_requests: 0,
};

describe("Q Switch core screen", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(proxyApi.getProxyStatus).mockResolvedValue(status);
    vi.mocked(qoderRouteApi.listRoutes).mockResolvedValue([]);
  });
  it("loads the core screen without scheduling background polling", async () => {
    const timer = vi.spyOn(window, "setInterval");
    render(<App />);
    expect(timer).not.toHaveBeenCalled();
    timer.mockRestore();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "启动" })).toBeEnabled(),
    );
    expect(screen.getByText("供应商模型配置")).toBeInTheDocument();
    expect(screen.getByText("原生模型接入")).toBeInTheDocument();
  });
  it("starts and stops only the Qoder gateway", async () => {
    render(<App />);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "启动" })).toBeEnabled(),
    );
    vi.mocked(proxyApi.getProxyStatus).mockResolvedValue({
      ...status,
      running: true,
    });
    fireEvent.click(screen.getByRole("button", { name: "启动" }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "停止" })).toBeEnabled(),
    );
    expect(proxyApi.startProxyServer).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    await waitFor(() =>
      expect(proxyApi.stopProxyWithRestore).toHaveBeenCalledOnce(),
    );
  });
  it("shows backend initialization errors and supports an explicit retry", async () => {
    vi.mocked(proxyApi.getProxyStatus).mockRejectedValueOnce(
      "port unavailable",
    );
    render(<App />);
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "port unavailable",
    );
    fireEvent.click(screen.getByRole("button", { name: "刷新状态" }));
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
  });
});
