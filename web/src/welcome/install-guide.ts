/**
 * The install assistant's output: a paste-ready shell block for the choices
 * made. Pure, so every combination is tested. The whole block runs in one
 * `set -eu` subshell, with no `a && b` where `a` matters (`set -e` ignores a
 * failure left of `&&`): pasted line by line, a failed verification or download
 * would otherwise be followed by the steps that install the bad file.
 */

const REPO = "mrn2se/AgenticWebInstrumentControl";
const RELEASE = `https://github.com/${REPO}/releases/latest/download`;
const UDEV_URL = `https://raw.githubusercontent.com/${REPO}/main/tools/udev/99-awic.rules`;

export type InstallArch = "auto" | "x86_64" | "aarch64" | "armv7";
export type InstallRun = "once" | "systemd";
/**
 * `demo` runs the virtual bench on a PC: no hardware, no root, no permissions, no service.
 * `uninstall` removes what a new install of the same kind put there.
 */
export type InstallAction = "install" | "update" | "demo" | "uninstall";

export interface InstallChoices {
  readonly action: InstallAction;
  readonly run: InstallRun;
  readonly arch: InstallArch;
  /**
   * Install the firmware package and the udev rules first. Only a new install needs it; a demo never
   * does. For an uninstall: remove the udev rules too.
   */
  readonly prerequisites: boolean;
  /** Verify the download's GitHub build attestation before using it. */
  readonly attestation: boolean;
  /** Listen on every network interface instead of this machine only. */
  readonly network: boolean;
}

export const DEFAULT_CHOICES: InstallChoices = {
  action: "install",
  run: "once",
  arch: "auto",
  prerequisites: true,
  attestation: false,
  network: false,
};

const UNIT = (network: boolean): string => `[Unit]
Description=AWIC bench instrument server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/awic
DynamicUser=yes
SupplementaryGroups=plugdev dialout
StateDirectory=awic
Environment=AWIC_SERVER__STATE_DIR=/var/lib/awic${network ? "\nEnvironment=AWIC_SERVER__BIND=0.0.0.0:8080" : ""}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target`;

function assetSelection(arch: InstallArch): string[] {
  if (arch !== "auto") return [`asset=awic-linux-${arch}`];
  return [
    `case "$(uname -m)" in`,
    `  x86_64) asset=awic-linux-x86_64 ;;`,
    `  aarch64|arm64) asset=awic-linux-aarch64 ;;`,
    `  armv7l|armv8l) asset=awic-linux-armv7 ;;`,
    `  *) echo "No AWIC build for $(uname -m)" >&2; exit 1 ;;`,
    `esac`,
  ];
}

/** Whether choices leave the group change pending: a new login is needed before a hand-started awic can use it. */
function needsNewLogin(c: InstallChoices): boolean {
  return c.action === "install" && c.prerequisites && c.run === "once";
}

/**
 * The demo: download the PC build into its own folder and start the virtual
 * bench. Only the attestation choice applies; it is kept apart from the real
 * install so nothing there (sudo, udev, systemd, network) can leak into it.
 */
function buildDemoScript(c: InstallChoices): string {
  const lines: string[] = ["(", "set -eu", ""];
  lines.push(
    "# The full AWIC program for a 64-bit PC; the demo is only how it is started below",
    "asset=awic-linux-x86_64",
    "mkdir -p awic",
    "cd awic",
    `curl -fL -o "$asset.new" "${RELEASE}/$asset"`,
    "",
  );
  if (c.attestation) {
    lines.push(
      "# Stop unless GitHub Actions in this repository built exactly this file.",
      "# Needs the GitHub CLI, signed in (gh auth login).",
      `gh attestation verify "$asset.new" --repo ${REPO}`,
      "",
    );
  }
  lines.push(
    `chmod +x "$asset.new"`,
    `mv "$asset.new" "$asset"`,
    "",
    "# Demo mode: a simulated board and instruments only. No hardware, no root,",
    "# no USB permissions, nothing left running in the background.",
  );
  lines.push(`exec "./$asset" --sim --set backends.native=false`, ")");
  return lines.join("\n");
}

/**
 * Undo a new install: the executable, its state and, if asked, the udev rules. The group
 * memberships and the packages stay, as other programs use them too.
 */
function buildUninstallScript(c: InstallChoices): string {
  const lines: string[] = ["(", "set -eu", "", "# Every removal asks first; answer y to remove, anything else to keep it.", ""];
  // rm asks once for a folder (-I) and once for a file (-i). Its folder question does not name the
  // folder, so the name goes first. Without -f rm fails on a missing path, so each one is checked
  // first and a second run does not stop halfway.
  const rm = (path: string, folder: boolean, sudo: boolean): string =>
    `if [ -e ${path} ]; then ${folder ? `echo ${path}; ` : ""}${sudo ? "sudo " : ""}rm ${folder ? "-rI" : "-i"} ${path}; fi`;
  if (c.run === "once") {
    lines.push(
      "# Stop here if AWIC still runs; run this where the awic folder is.",
      "if pgrep '^awic-linux-' >/dev/null; then",
      `  echo "AWIC is still running. Stop it (Ctrl+C in its terminal), then run this again:" >&2`,
      "  pgrep -a '^awic-linux-' >&2",
      "  exit 1",
      "fi",
      rm("awic", true, false),
      "",
      "# Its saved USB port locks",
      rm(`"\${XDG_STATE_HOME:-$HOME/.local/state}/awic"`, true, false),
    );
  } else {
    lines.push(
      "# Stop the service and remove it, with its state (saved USB port locks)",
      "if systemctl cat awic >/dev/null 2>&1; then",
      "  sudo systemctl disable --now awic",
      "  sudo systemctl clean --what=state awic",
      `  ${rm("/etc/systemd/system/awic.service", false, true)}`,
      "  sudo systemctl daemon-reload",
      "fi",
      rm("/usr/local/bin/awic", false, true),
    );
  }
  if (c.prerequisites) {
    lines.push(
      "",
      "# The USB permissions",
      rm("/etc/udev/rules.d/99-awic.rules", false, true),
      "sudo udevadm control --reload-rules",
      "sudo udevadm trigger",
    );
  }
  lines.push(
    "",
    "# Left in place, as other programs use them too: the packages sigrok-firmware-fx2lafw",
    "# and fonts-dejavu-core, and the group memberships:",
    `echo "Warning: user groups were not touched. A new install started by hand adds its user to" >&2`,
    `echo "plugdev and dialout; take them out by hand if nothing else needs those groups." >&2`,
    ")",
  );
  return lines.join("\n");
}

export function buildInstallScript(c: InstallChoices): string {
  if (c.action === "demo") return buildDemoScript(c);
  if (c.action === "uninstall") return buildUninstallScript(c);
  const lines: string[] = ["(", "set -eu", ""];
  const prerequisites = c.action === "install" && c.prerequisites;

  if (prerequisites) {
    lines.push(
      "# Firmware for the instruments and a font for image labels (Debian, Ubuntu)",
      "sudo apt-get install -y sigrok-firmware-fx2lafw fonts-dejavu-core",
      "",
      "# USB access without root: instruments, hubs and port power for the plugdev group.",
      "# Read the rules first. As they are, they cover every matching device and hub.",
      `rules="$(mktemp)"`,
      `curl -fsSL -o "$rules" ${UDEV_URL}`,
      `sudo install -m 644 "$rules" /etc/udev/rules.d/99-awic.rules`,
      `rm -f "$rules"`,
      "sudo udevadm control --reload-rules",
      "sudo udevadm trigger",
    );
    if (c.run === "once") lines.push("# plugdev for the rules above, dialout for serial ports", `sudo usermod -aG plugdev,dialout "$USER"`);
    lines.push("");
  }

  lines.push("# The executable for this machine", ...assetSelection(c.arch));
  if (c.run === "once") lines.push("mkdir -p awic", "cd awic");
  else lines.push(`cd "$(mktemp -d)"`);
  lines.push(`curl -fL -o "$asset.new" "${RELEASE}/$asset"`, "");

  if (c.attestation) {
    lines.push(
      "# Stop unless GitHub Actions in this repository built exactly this file.",
      "# Needs the GitHub CLI, signed in (gh auth login).",
      `gh attestation verify "$asset.new" --repo ${REPO}`,
      "",
    );
  }

  if (c.run === "once") {
    lines.push(`chmod +x "$asset.new"`, `mv "$asset.new" "$asset"`);
    if (c.action === "update") {
      lines.push("", "# Stop the running copy first, then start this one.");
    }
    if (!needsNewLogin(c)) {
      lines.push(
        "",
        c.network
          ? `# Listens on every interface: anyone on your network can drive the bench.\nexec "./$asset" --bind 0.0.0.0:8080`
          : `exec "./$asset"`,
      );
    }
  } else {
    lines.push("# Install the executable");
    if (c.action === "update") lines.push("sudo systemctl stop awic");
    lines.push(`sudo install -m 755 "$asset.new" /usr/local/bin/awic`, `rm -f "$asset.new"`, "");
    if (c.action === "install") {
      if (c.network) lines.push("# Listens on every interface: anyone on your network can drive the bench.");
      lines.push(
        "# The service: its own unprivileged user, state in /var/lib/awic",
        "sudo tee /etc/systemd/system/awic.service >/dev/null <<'UNIT'",
        UNIT(c.network),
        "UNIT",
        "sudo systemctl daemon-reload",
        "sudo systemctl enable --now awic",
      );
    } else {
      lines.push("sudo systemctl start awic");
    }
    lines.push("systemctl --no-pager --lines=0 status awic");
  }
  lines.push(")");

  if (needsNewLogin(c)) {
    const run = c.network ? `./${c.arch === "auto" ? "awic-linux-*" : `awic-linux-${c.arch}`} --bind 0.0.0.0:8080` : `./${c.arch === "auto" ? "awic-linux-*" : `awic-linux-${c.arch}`}`;
    lines.push("", "# Then log out and back in, replug the instruments, and start it:", `#   cd awic; ${run}`);
  }
  return lines.join("\n");
}

/** What to do once the block has run, in one sentence. */
export function afterInstallNote(c: InstallChoices): string {
  if (c.action === "uninstall") {
    return c.run === "systemd"
      ? "AWIC is gone once the block has run; nothing is left running."
      : "Run it in the folder that holds the awic folder. AWIC is gone once the block has run.";
  }
  const where = c.network ? "http://<this machine>:8080/" : "http://127.0.0.1:8080/";
  if (c.action === "demo") {
    return `Open http://127.0.0.1:8080/ and the Bench tab while it runs. Stop it with Ctrl+C; delete the awic folder to remove it.`;
  }
  if (c.run === "systemd") {
    return `AWIC starts with the machine. Open ${where} once it is running. Logs: journalctl -u awic -f`;
  }
  return `Open ${where} while it runs. Stop it with Ctrl+C.`;
}
