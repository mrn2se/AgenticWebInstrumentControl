import { describe, expect, it } from "vitest";
import {
  claudeCodeCommand,
  connectionSnippets,
  mcpEndpointUrl,
  reachabilityNote,
  vscodeHttpConfig,
  vscodeStdioConfig,
} from "./connection";

describe("mcpEndpointUrl", () => {
  it("is this page's origin plus /mcp", () => {
    expect(mcpEndpointUrl({ protocol: "http:", host: "bench.local:8080" })).toBe("http://bench.local:8080/mcp");
    expect(mcpEndpointUrl({ protocol: "https:", host: "awic.example" })).toBe("https://awic.example/mcp");
  });
});

describe("vscodeHttpConfig", () => {
  it("is a plain http server entry without a token", () => {
    const config = JSON.parse(vscodeHttpConfig("http://h:8080/mcp", false));
    expect(config).toEqual({ servers: { awic: { type: "http", url: "http://h:8080/mcp" } } });
  });

  it("prompts for the token rather than writing it into the file", () => {
    const config = JSON.parse(vscodeHttpConfig("http://h:8080/mcp", true));
    expect(config.inputs[0]).toMatchObject({ type: "promptString", id: "awic-token", password: true });
    expect(config.servers.awic.headers.Authorization).toBe("Bearer ${input:awic-token}");
  });
});

describe("vscodeStdioConfig", () => {
  it("runs mcp-stdio inside the container", () => {
    const config = JSON.parse(vscodeStdioConfig(false));
    expect(config.servers.awic).toEqual({
      type: "stdio",
      command: "podman",
      args: ["exec", "-i", "awic", "awic", "mcp-stdio"],
    });
  });

  it("passes the token through the environment when one is required", () => {
    const config = JSON.parse(vscodeStdioConfig(true));
    expect(config.servers.awic.args).toContain("AWIC_MCP_TOKEN=${input:awic-token}");
    expect(config.inputs).toHaveLength(1);
  });
});

describe("claudeCodeCommand", () => {
  it("adds an http server, with a header placeholder when a token is required", () => {
    expect(claudeCodeCommand("http://h/mcp", false)).toBe("claude mcp add --transport http awic http://h/mcp");
    expect(claudeCodeCommand("http://h/mcp", true)).toContain('--header "Authorization: Bearer <token>"');
  });
});

describe("connectionSnippets", () => {
  it("offers VS Code first", () => {
    const snippets = connectionSnippets("http://h/mcp", false);
    expect(snippets.map((s) => s.id)).toEqual(["vscode", "vscode-stdio", "claude"]);
  });
});

describe("reachabilityNote", () => {
  it("warns when both the bind and the page are loopback", () => {
    expect(reachabilityNote({ protocol: "http:", host: "127.0.0.1:8080" }, "127.0.0.1:8080")).toContain("this machine only");
  });

  it("says nothing for a server reachable on the network", () => {
    expect(reachabilityNote({ protocol: "http:", host: "bench:8080" }, "0.0.0.0:8080")).toBeNull();
  });
});
