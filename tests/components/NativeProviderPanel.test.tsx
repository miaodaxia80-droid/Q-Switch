import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { describe, it, expect, vi, beforeEach } from "vitest";
import { NativeProviderPanel } from "@/components/qoder/NativeProviderPanel";
import { qoderRouteApi } from "@/lib/api/qoderRoute";
import { toast } from "sonner";

vi.mock("@/lib/api/qoderRoute", () => ({
  qoderRouteApi: { syncNativeProvider: vi.fn(), removeNativeProvider: vi.fn() },
}));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));
// Native selects make the test independent of Radix pointer-event polyfills.
vi.mock("@/components/ui/select", () => ({
  Select: ({ value, onValueChange, children, disabled }: any) => (
    <select
      value={value}
      onChange={(e) => onValueChange(e.target.value)}
      disabled={disabled}
    >
      {children}
    </select>
  ),
  SelectTrigger: () => null,
  SelectValue: () => null,
  SelectContent: ({ children }: any) => children,
  SelectItem: ({ value, children }: any) => (
    <option value={value}>{children}</option>
  ),
}));

const routes = [
  {
    routeId: "qswitch_p_m",
    providerId: "p",
    model: "upstream",
    displayName: "My model",
  },
];

describe("NativeProviderPanel", () => {
  beforeEach(() => vi.resetAllMocks());

  it("requires a live gateway and an explicitly selected available route", () => {
    const { rerender } = render(
      <NativeProviderPanel routes={routes} isRunning={false} />,
    );
    fireEvent.change(screen.getAllByRole("combobox")[1], {
      target: { value: routes[0].routeId },
    });
    expect(
      screen.getByRole("button", { name: "同步模型并选择" }),
    ).toBeDisabled();
    rerender(<NativeProviderPanel routes={routes} isRunning />);
    expect(
      screen.getByRole("button", { name: "同步模型并选择" }),
    ).toBeEnabled();
    rerender(<NativeProviderPanel routes={[]} isRunning />);
    expect(
      screen.getByRole("button", { name: "同步模型并选择" }),
    ).toBeDisabled();
  });

  it("passes the CN target and stable route ID to native configuration", async () => {
    vi.mocked(qoderRouteApi.syncNativeProvider).mockResolvedValue(
      "/test/settings.json",
    );
    render(<NativeProviderPanel routes={routes} isRunning />);
    const [target, model] = screen.getAllByRole("combobox");
    fireEvent.change(target, { target: { value: "qoder-cn" } });
    fireEvent.change(model, { target: { value: routes[0].routeId } });
    fireEvent.click(screen.getByRole("button", { name: "同步模型并选择" }));
    await waitFor(() =>
      expect(qoderRouteApi.syncNativeProvider).toHaveBeenCalledWith(
        "qoder-cn",
        "qswitch_p_m",
      ),
    );
    expect(toast.success).toHaveBeenCalled();
  });

  it("allows restoring the configuration after the gateway stops", async () => {
    vi.mocked(qoderRouteApi.removeNativeProvider).mockResolvedValue(
      "/test/settings.json",
    );
    render(<NativeProviderPanel routes={[]} isRunning={false} />);
    fireEvent.click(screen.getByRole("button", { name: "移除并恢复选择" }));
    await waitFor(() =>
      expect(qoderRouteApi.removeNativeProvider).toHaveBeenCalledWith("qoder"),
    );
  });

  it("reports configuration conflicts and allows retry", async () => {
    vi.mocked(qoderRouteApi.removeNativeProvider).mockRejectedValue(
      new Error("provider conflict"),
    );
    render(<NativeProviderPanel routes={[]} isRunning={false} />);
    fireEvent.click(screen.getByRole("button", { name: "移除并恢复选择" }));
    await waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith(
        expect.stringContaining("provider conflict"),
      ),
    );
    expect(
      screen.getByRole("button", { name: "移除并恢复选择" }),
    ).toBeEnabled();
  });
});
