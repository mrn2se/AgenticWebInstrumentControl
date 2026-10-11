import { describe, expect, it } from "vitest";
import { lockChangeNeedsConfirmation, lockChoice, lockViewModel, protectedValueForChoice } from "./lock-view";
import { PortProtectTracker } from "./port-protect";
import { portKey } from "./port-power";
import { UsbTreeModel, flattenTree } from "./tree-model";
import { benchTree } from "./fixtures";

const IDLE = { phase: "idle" as const };

describe("lockChoice — the three states, from user_choice", () => {
  it("maps true/false/absent to locked/unlocked/automatic", () => {
    expect(lockChoice(true)).toBe("locked");
    expect(lockChoice(false)).toBe("unlocked");
    expect(lockChoice(null)).toBe("automatic");
  });
});

describe("protectedValueForChoice — the usb.protect request field for each state", () => {
  it("sends true, false, and null respectively", () => {
    expect(protectedValueForChoice("locked")).toBe(true);
    expect(protectedValueForChoice("unlocked")).toBe(false);
    expect(protectedValueForChoice("automatic")).toBeNull();
  });
});

describe("lockChangeNeedsConfirmation — the only guardrail against a one-click hub cut (SPEC §4.4)", () => {
  const hubReason = "feeds hub 3-3.4: switching it cuts every device behind it";

  it("requires confirmation when unlocking a port the rule protects", () => {
    expect(lockChangeNeedsConfirmation({ derivedProtection: hubReason, userChoice: null }, "unlocked")).toBe(true);
  });

  it("still requires it when the user had locked that hub port explicitly first", () => {
    // The regression: keyed on "automatic", locked -> unlocked removed the
    // hub protection with one unconfirmed click.
    expect(lockChangeNeedsConfirmation({ derivedProtection: hubReason, userChoice: true }, "unlocked")).toBe(true);
  });

  it("does not ask to unlock a programmer the user locked — the rule never protected it", () => {
    expect(lockChangeNeedsConfirmation({ derivedProtection: null, userChoice: true }, "unlocked")).toBe(false);
  });

  it("does not ask when the port is already unlocked", () => {
    expect(lockChangeNeedsConfirmation({ derivedProtection: hubReason, userChoice: false }, "unlocked")).toBe(false);
  });

  it("never asks to lock or to return to automatic", () => {
    for (const userChoice of [null, true, false]) {
      expect(lockChangeNeedsConfirmation({ derivedProtection: hubReason, userChoice }, "locked")).toBe(false);
      expect(lockChangeNeedsConfirmation({ derivedProtection: hubReason, userChoice }, "automatic")).toBe(false);
    }
  });
});

describe("lockViewModel", () => {
  const rows = flattenTree(benchTree());
  const byKey = new Map(rows.map((r) => [r.key, r]));

  it("reports automatic/protected with the reason visible for a rule-protected port", () => {
    const port4 = byKey.get("3-3.4");
    if (!port4) throw new Error("fixture missing 3-3.4");
    const view = lockViewModel(port4, IDLE);
    expect(view.choice).toBe("automatic");
    expect(view.protected).toBe(true);
    expect(view.protectionReason).toBe("feeds hub 3-3.4: switching it cuts every device behind it");
    expect(view.pending).toBe(false);
    expect(view.errorMessage).toBeNull();
  });

  it("reports automatic/unprotected for an ordinary switchable port", () => {
    const port3 = byKey.get("3-3.3");
    if (!port3) throw new Error("fixture missing 3-3.3");
    const view = lockViewModel(port3, IDLE);
    expect(view.choice).toBe("automatic");
    expect(view.protected).toBe(false);
    expect(view.protectionReason).toBeNull();
  });

  it("renders pending while a usb.protect request is outstanding", () => {
    const port3 = byKey.get("3-3.3");
    if (!port3) throw new Error("fixture missing 3-3.3");
    const view = lockViewModel(port3, { phase: "pending" });
    expect(view.pending).toBe(true);
  });

  it("surfaces a failed usb.protect distinctly from the protection reason", () => {
    const port4 = byKey.get("3-3.4");
    if (!port4) throw new Error("fixture missing 3-3.4");
    const view = lockViewModel(port4, { phase: "error", error: { code: "state_write_failed", msg: "disk full" } });
    expect(view.errorMessage).toBe("state_write_failed: disk full");
    expect(view.protectionReason).toBe("feeds hub 3-3.4: switching it cuts every device behind it");
  });
});

describe("a protection change is never optimistic — only a fresh usb.tree apply() moves it (SPEC §11)", () => {
  it("leaves PortRow.protected/userChoice untouched while a usb.protect request is pending", () => {
    const model = new UsbTreeModel();
    const tracker = new PortProtectTracker();

    const before = model.apply(benchTree());
    const row = before.rows.find((r) => r.key === "3-3.4");
    if (!row) throw new Error("fixture missing 3-3.4");
    expect(row.protected).toBe(true);
    expect(row.userChoice).toBeNull();

    // User confirms unlocking. A request goes out; nothing about the tree
    // model changes yet — only the tracker's pending flag.
    if (row.parentHub === null || row.portNumber === null) throw new Error("fixture row 3-3.4 unexpectedly has no toggle");
    const key = portKey(row.parentHub, row.portNumber);
    tracker.begin(key);
    expect(model.get("3-3.4")?.protected).toBe(true);
    expect(model.get("3-3.4")?.userChoice).toBeNull();
    expect(tracker.get(key).phase).toBe("pending");

    // The server publishes usb.changed { change: "protection" }; the module
    // re-fetches usb.tree, which is the only thing that actually moves the row.
    const confirmed = benchTree();
    const upstream = confirmed.roots[0];
    if (!upstream) throw new Error("fixture missing root");
    const port4 = upstream.ports.find((p) => p.number === 4);
    if (!port4) throw new Error("fixture missing port 4");
    port4.protected = false;
    port4.protection = null;
    port4.user_choice = false;
    port4.switchable = true;
    port4.non_switchable_reason = null;

    const after = model.apply(confirmed);
    tracker.resolve(key);

    const updated = after.rows.find((r) => r.key === "3-3.4");
    expect(updated?.protected).toBe(false);
    expect(updated?.userChoice).toBe(false);
    expect(updated?.switchable).toBe(true);
    expect(tracker.get(key).phase).toBe("idle");
  });
});
