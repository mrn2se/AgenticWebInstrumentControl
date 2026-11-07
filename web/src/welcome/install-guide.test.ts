// @ts-ignore: the project has no Node type definitions; this test alone runs a child process.
import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { type InstallChoices, DEFAULT_CHOICES, buildInstallScript } from "./install-guide";

const script = (over: Partial<InstallChoices>): string => buildInstallScript({ ...DEFAULT_CHOICES, ...over });

function allChoices(): InstallChoices[] {
  const out: InstallChoices[] = [];
  for (const action of ["install", "update", "demo", "uninstall"] as const)
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

  it("never lets set -e miss a failure, and never uses a predictable temp file", () => {
    for (const choices of allChoices()) {
      const text = buildInstallScript(choices);
      expect(text).not.toContain("&&");
      expect(text).not.toContain("/tmp/");
    }
  });

  it("gives the service and the user access to serial ports", () => {
    expect(script({ run: "systemd" })).toContain("SupplementaryGroups=plugdev dialout");
    expect(script({ run: "once" })).toContain("usermod -aG plugdev,dialout");
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

  it("runs a demo as the virtual bench, without sudo, permissions or a service", () => {
    for (const choices of allChoices().filter((c) => c.action === "demo")) {
      const text = buildInstallScript(choices);
      expect(text).toContain("--sim --set backends.native=false");
      expect(text).toContain("asset=awic-linux-x86_64");
      expect(text).not.toContain("uname");
      expect(text).not.toContain("0.0.0.0");
      expect(text).toContain("cd awic\n");
      for (const absent of ["sudo", "udev", "usermod", "systemctl", "apt-get", "sigrok-firmware"]) expect(text).not.toContain(absent);
      expect(text).toContain("exec ");
    }
  });

  it("verifies a demo download before running it, only when asked", () => {
    const text = script({ action: "demo", attestation: true });
    expect(text.indexOf("gh attestation verify")).toBeLessThan(text.indexOf("chmod +x"));
    expect(script({ action: "demo" })).not.toContain("gh attestation");
  });

  it("uninstalls what a hand install put there", () => {
    const text = script({ action: "uninstall", run: "once", prerequisites: false });
    expect(text).toContain("echo awic; rm -rI awic;");
    expect(text).toContain('rm -rI "${XDG_STATE_HOME:-$HOME/.local/state}/awic";');
    expect(text).not.toContain("sudo");
    expect(text).not.toContain("curl");
  });

  it("stops a hand uninstall with an error while AWIC runs, before removing anything", () => {
    const text = script({ action: "uninstall", run: "once" });
    const check = text.indexOf("if pgrep '^awic-linux-'");
    expect(check).toBeGreaterThan(-1);
    expect(check).toBeLessThan(text.indexOf("rm -"));
    expect(text.slice(check)).toMatch(/>&2\n\s*exit 1\nfi/);
  });

  it("uninstalls the service, its state and its executable", () => {
    const text = script({ action: "uninstall", run: "systemd", prerequisites: false });
    expect(text.indexOf("systemctl disable --now awic")).toBeLessThan(text.indexOf("systemctl clean --what=state awic"));
    expect(text.indexOf("systemctl clean --what=state awic")).toBeLessThan(text.indexOf("rm -i /etc/systemd/system/awic.service"));
    expect(text).toContain("sudo rm -i /usr/local/bin/awic");
    expect(text).not.toContain("curl");
  });

  it("asks before every removal, and skips what is already gone", () => {
    for (const run of ["once", "systemd"] as const) {
      const text = script({ action: "uninstall", run, prerequisites: true });
      expect(text).not.toMatch(/rm -[a-zA-Z]*f/);
      for (const line of text.split("\n").filter((l) => /\brm /.test(l))) expect(line.trim()).toMatch(/^if \[ -e .+ \]; then (echo .+; )?(sudo )?rm -(rI|i) .+; fi$/);
    }
  });

  it("removes the udev rules only when asked, and never the groups or packages", () => {
    for (const run of ["once", "systemd"] as const) {
      expect(script({ action: "uninstall", run, prerequisites: true })).toContain("sudo rm -i /etc/udev/rules.d/99-awic.rules");
      expect(script({ action: "uninstall", run, prerequisites: false })).not.toContain("99-awic.rules");
      const text = script({ action: "uninstall", run, prerequisites: true });
      expect(text).not.toContain("apt-get");
      expect(text).not.toContain("gpasswd");
      expect(text).not.toContain("usermod");
      expect(text).toContain('echo "Warning: user groups were not touched.');
    }
  });
});
