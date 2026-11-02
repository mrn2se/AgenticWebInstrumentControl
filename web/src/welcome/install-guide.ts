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
export type InstallAction = "install" | "update";

export interface InstallChoices {
  readonly action: InstallAction;
  readonly run: InstallRun;
  readonly arch: InstallArch;
  /** Install the firmware package and the udev rules first. Only a new install needs it. */
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

export function buildInstallScript(c: InstallChoices): string {
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
  const where = c.network ? "http://<this machine>:8080/" : "http://127.0.0.1:8080/";
  if (c.run === "systemd") {
    return `AWIC starts with the machine. Open ${where} once it is running. Logs: journalctl -u awic -f`;
  }
  return `Open ${where} while it runs. Stop it with Ctrl+C.`;
}
