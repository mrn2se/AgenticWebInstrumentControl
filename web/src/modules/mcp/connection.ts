/**
 * What a person copies into an MCP client to reach this server
 * (SPECIFICATION.md §8.1, §8.5). Pure, so the snippets are unit-tested
 * exactly as they are shown.
 */

/** The parts of `window.location` the endpoint URL is built from. */
export interface PageLocation {
  readonly protocol: string;
  readonly host: string;
}

/** The streamable-HTTP endpoint as reached from this page's own origin. */
export function mcpEndpointUrl(location: PageLocation, path = "/mcp"): string {
  return `${location.protocol}//${location.host}${path}`;
}

/** The `podman` container name `tools/run.sh`/`tools/run-rootful.sh` start AWIC under. */
export const CONTAINER_NAME = "awic";

/** Name the server is registered under in every snippet. */
export const SERVER_KEY = "awic";

/** Id of the VS Code input that prompts for the token, when one is configured. */
export const TOKEN_INPUT_ID = "awic-token";

export interface Snippet {
  readonly id: string;
  readonly label: string;
  /** Where the text goes, shown above it. */
  readonly where: string;
  readonly text: string;
}

/**
 * `.vscode/mcp.json` for the HTTP transport. With a token configured the
 * token is never written into the file: VS Code prompts for it once
 * (`promptString`, `password: true`) and keeps it in its secret storage.
 */
export function vscodeHttpConfig(url: string, authRequired: boolean): string {
  const server: Record<string, unknown> = { type: "http", url };
  const config: Record<string, unknown> = {};
  if (authRequired) {
    config.inputs = [
      { type: "promptString", id: TOKEN_INPUT_ID, description: "AWIC bearer token", password: true },
    ];
    server.headers = { Authorization: `Bearer \${input:${TOKEN_INPUT_ID}}` };
  }
  config.servers = { [SERVER_KEY]: server };
  return JSON.stringify(config, null, 2);
}

/**
 * `.vscode/mcp.json` for clients that only speak stdio: `awic mcp-stdio`
 * inside the running container proxies to its own `/mcp`.
 */
export function vscodeStdioConfig(authRequired: boolean): string {
  const args = ["exec", "-i"];
  if (authRequired) args.push("-e", `AWIC_MCP_TOKEN=\${input:${TOKEN_INPUT_ID}}`);
  args.push(CONTAINER_NAME, "awic", "mcp-stdio");
  const config: Record<string, unknown> = {};
  if (authRequired) {
    config.inputs = [
      { type: "promptString", id: TOKEN_INPUT_ID, description: "AWIC bearer token", password: true },
    ];
  }
  config.servers = { [SERVER_KEY]: { type: "stdio", command: "podman", args } };
  return JSON.stringify(config, null, 2);
}

/** The Claude Code CLI one-liner. */
export function claudeCodeCommand(url: string, authRequired: boolean): string {
  const header = authRequired ? ` --header "Authorization: Bearer <token>"` : "";
  return `claude mcp add --transport http${header} ${SERVER_KEY} ${url}`;
}

/** Every snippet the connection panel offers, in display order. */
export function connectionSnippets(url: string, authRequired: boolean): Snippet[] {
  return [
    {
      id: "vscode",
      label: "VS Code",
      where: ".vscode/mcp.json in your workspace (or “MCP: Open User Configuration”)",
      text: vscodeHttpConfig(url, authRequired),
    },
    {
      id: "vscode-stdio",
      label: "VS Code (stdio)",
      where: ".vscode/mcp.json, on the machine running the container",
      text: vscodeStdioConfig(authRequired),
    },
    {
      id: "claude",
      label: "Claude Code",
      where: "a terminal",
      text: claudeCodeCommand(url, authRequired),
    },
  ];
}

/**
 * A note when the URL as seen from this page will not work from elsewhere:
 * a server bound to loopback is only reachable from the same machine.
 */
export function reachabilityNote(location: PageLocation, bind: string): string | null {
  const loopbackBind = /^(127\.|localhost|\[::1\])/.test(bind);
  const loopbackPage = /^(127\.|localhost|\[::1\])/.test(location.host);
  if (loopbackBind && loopbackPage) {
    return `The server is bound to ${bind}: MCP clients on this machine only. Set [server].bind to reach it from elsewhere.`;
  }
  return null;
}
