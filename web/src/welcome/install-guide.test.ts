// @ts-ignore: the project has no Node type definitions; this test alone runs a child process.
import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { type InstallChoices, DEFAULT_CHOICES, buildInstallScript } from "./install-guide";

const script = (over: Partial<InstallChoices>): string => buildInstallScript({ ...DEFAULT_CHOICES, ...over });

function allChoices(): InstallChoices[] {
  const out: InstallChoices[] = [];
  for (const action of ["install", "update"] as const)
    for (const run of ["once", "systemd"] as const)
      for (const arch of ["auto", "x86_64", "aarch64", "armv7"] as const)
        for (const prerequisites of [true, false])
          for (const attestation of [true, false])
            for (const network of [true, false]) out.push({ action, run, arch, prerequisites, attestation, network });
  return out;
}

describe("buildInstallScript", () => {
  it("is valid shell for every combination", () => {
    for (const choices of allChoices()) {
      const result = spawnSync("sh", ["-n"], { input: buildInstallScript(choices), encoding: "utf8" });
      expect(result.status, `${JSON.stringify(choices)}: ${result.stderr}`).toBe(0);
    }
  });

  it("runs everything in one stop-on-error subshell", () => {
    const lines = script({}).split("\n");
    expect(lines[0]).toBe("(");
    expect(lines[1]).toBe("set -eu");
  });

  it("installs prerequisites only on request, and never for an update", () => {
    expect(script({ prerequisites: true })).toContain("sigrok-firmware-fx2lafw");
    expect(script({ prerequisites: false })).not.toContain("sigrok-firmware-fx2lafw");
    expect(script({ action: "update", prerequisites: true })).not.toContain("sigrok-firmware-fx2lafw");
  });

  it("detects the machine, or uses the chosen file", () => {
    expect(script({})).toContain('case "$(uname -m)"');
    const fixed = script({ arch: "aarch64" });
    expect(fixed).not.toContain("uname -m");
    expect(fixed).toContain("asset=awic-linux-aarch64");
  });

  it("verifies before anything is installed", () => {
    const text = script({ run: "systemd", attestation: true });
    const verify = text.indexOf("gh attestation verify");
    expect(verify).toBeGreaterThan(-1);
    expect(verify).toBeLessThan(text.indexOf("sudo install -m 755"));
    expect(script({ attestation: false })).not.toContain("gh attestation");
  });

  it("writes a service for a new systemd install, and only restarts it for an update", () => {
    const fresh = script({ run: "systemd" });
    expect(fresh).toContain("/etc/systemd/system/awic.service");
    expect(fresh).toContain("systemctl enable --now awic");
    const update = script({ run: "systemd", action: "update" });
    expect(update).not.toContain("awic.service");
    expect(update.indexOf("systemctl stop awic")).toBeLessThan(update.indexOf("sudo install -m 755"));
    expect(update).toContain("systemctl start awic");
  });

  it("binds to the network only when asked", () => {
    expect(script({ prerequisites: false })).not.toContain("0.0.0.0");
    expect(script({ prerequisites: false, network: true })).toContain("--bind 0.0.0.0:8080");
    expect(script({ run: "systemd", network: true })).toContain("AWIC_SERVER__BIND=0.0.0.0:8080");
  });

  it("does not start a hand-run awic before the new group is in effect", () => {
    const text = script({ run: "once", prerequisites: true });
    expect(text).toContain("usermod -aG plugdev");
    expect(text).not.toContain("exec ");
    expect(text).toContain("log out and back in");
  });
});
