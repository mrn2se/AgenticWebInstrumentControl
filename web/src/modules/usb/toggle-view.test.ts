import { describe, expect, it } from "vitest";
import { flattenTree } from "./tree-model";
import { toggleViewModel } from "./toggle-view";
import { benchTree } from "./fixtures";

const IDLE = { phase: "idle" as const };

describe("toggleViewModel — disabled-with-reason rendering (SPEC §4.4, §11)", () => {
  const rows = flattenTree(benchTree());
  const byKey = new Map(rows.map((r) => [r.key, r]));

  it("renders a protected hub port disabled, with its reason visible", () => {
    const port4 = byKey.get("3-3.4");
    if (!port4) throw new Error("fixture missing 3-3.4");
    const view = toggleViewModel(port4, IDLE);
    expect(view.enabled).toBe(false);
    expect(view.reason).toBe("feeds hub 3-3.4: switching it cuts every device behind it");
  });

  it("renders the root hub's own (unmanaged) port disabled, with a reason, not hidden", () => {
    const root = byKey.get("3-3");
    if (!root) throw new Error("fixture missing 3-3");
    const view = toggleViewModel(root, IDLE);
    expect(view.enabled).toBe(false);
    expect(view.reason).toBeTruthy();
  });

  it("renders an unprotected, switchable port enabled with no reason", () => {
    const port3 = byKey.get("3-3.3");
    if (!port3) throw new Error("fixture missing 3-3.3");
    const view = toggleViewModel(port3, IDLE);
    expect(view.enabled).toBe(true);
    expect(view.reason).toBeNull();
  });

  it("renders an empty-but-unprotected port enabled — a user may still want to switch it", () => {
    const emptyPort = byKey.get("3-3.1");
    if (!emptyPort) throw new Error("fixture missing 3-3.1");
    const view = toggleViewModel(emptyPort, IDLE);
    expect(view.enabled).toBe(true);
    expect(view.reason).toBeNull();
  });

  it("renders pending state without changing the reported on/off value", () => {
    const port3 = byKey.get("3-3.3");
    if (!port3) throw new Error("fixture missing 3-3.3");
    const view = toggleViewModel(port3, { phase: "pending" });
    expect(view.pending).toBe(true);
    expect(view.on).toBe(port3.power);
  });

  it("surfaces a guardrail refusal message distinctly from the disabled-reason text", () => {
    const port3 = byKey.get("3-3.3");
    if (!port3) throw new Error("fixture missing 3-3.3");
    const view = toggleViewModel(port3, {
      phase: "error",
      error: { code: "port_protected", msg: "port 3 feeds the instrument hub" },
    });
    expect(view.enabled).toBe(true); // the row itself is still unprotected per the tree...
    expect(view.errorMessage).toBe("port_protected: port 3 feeds the instrument hub");
    expect(view.reason).toBeNull(); // ...so this is an attempt failure, not a static disabled-reason
  });
});
