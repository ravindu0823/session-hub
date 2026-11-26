import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { parseRequest } from "../../src/bridge/protocol.ts";

// Source scans for the rules in CLAUDE.md "Security rules (binding)" and "Domain rules" that no
// behaviour test can see: a new listener, a new bridge request type or a relay flag change must
// fail here first (audit-testing TST-01, TST-03).

const ROOT = join(import.meta.dir, "..", "..");

function files(dir: string, exts: string[]): string[] {
  const out: string[] = [];
  for (const e of readdirSync(join(ROOT, dir), { withFileTypes: true })) {
    const rel = join(dir, e.name);
    if (e.isDirectory()) {
      if (["bin", "obj", "node_modules", "RenderFixtures"].includes(e.name)) continue;
      out.push(...files(rel, exts));
    } else if (exts.some((x) => e.name.endsWith(x))) {
      out.push(rel);
    }
  }
  return out;
}

const posix = (p: string) => p.split(sep).join("/");
const read = (rel: string) => readFileSync(join(ROOT, rel), "utf8");
const TS = files("src", [".ts", ".tsx"]);
const CS = files("gui", [".cs"]);

/** Lines that are code, not comments (good enough for these patterns). */
function codeLines(text: string): string[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => !l.startsWith("//") && !l.startsWith("*") && !l.startsWith("/*"));
}

function hits(paths: string[], re: RegExp): string[] {
  const out: string[] = [];
  for (const p of paths) {
    codeLines(read(p)).forEach((l) => {
      if (re.test(l)) out.push(`${posix(p)}: ${l}`);
    });
  }
  return out;
}

describe("no network listener of any kind", () => {
  test("TypeScript opens no server, socket or HTTP client", () => {
    expect(
      hits(
        TS,
        /Bun\.serve|Bun\.listen|Bun\.connect|node:(?:net|http|https|http2|dgram|tls)\b|\bcreateServer\(|\.listen\(|new WebSocket|\bfetch\(/,
      ),
    ).toEqual([]);
  });

  test("the tray opens no listener, socket or pipe server", () => {
    expect(
      hits(
        CS,
        /HttpListener|TcpListener|UdpClient|new Socket\(|NamedPipeServerStream|WebSocket|HttpClient|Kestrel/,
      ),
    ).toEqual([]);
  });
});

describe("relay and peer messaging limits", () => {
  test("crossSessionInbound is never written and --restricted is never used", () => {
    expect(hits([...TS, ...CS], /crossSessionInbound|--restricted/)).toEqual([]);
  });

  test("session key files and the messaging socket are never named in code", () => {
    expect(hits(TS, /messagingSocketPath|\.key["'`]/)).toEqual([]);
  });
});

describe("bridge requests: exactly the reviewed set (no approval path)", () => {
  // "answer" stores the owner's answer to a decision (src/mail) and mails it to the role as peer text
  // marked unverified; it answers no permission prompt and approves nothing in any session.
  const KNOWN = ["history", "search", "send", "answer", "startDaemon", "ping", "shutdown"];

  test("the Request union names only the known types", () => {
    const src = read("src/bridge/protocol.ts");
    const union = src.slice(src.indexOf("export type Request ="), src.indexOf("/** A session as"));
    const types = [...union.matchAll(/type: "([A-Za-z]+)"/g)].map((m) => m[1]);
    expect([...new Set(types)].sort()).toEqual([...KNOWN].sort());
  });

  test("anything else is refused as unknown-type", () => {
    for (const type of ["approve", "permission", "allow", "exec", "kill", "focus", "install"]) {
      expect(parseRequest(JSON.stringify({ id: 1, type }))).toEqual({
        error: "unknown-type",
        id: 1,
      });
    }
  });
});

describe("import boundaries", () => {
  test("bun:ffi only in src/platform/windows", () => {
    const bad = TS.filter((p) => !posix(p).startsWith("src/platform/windows/")).filter((p) =>
      /from "bun:ffi"/.test(read(p)),
    );
    expect(bad).toEqual([]);
  });

  test("Windows platform code is imported only by composition roots; sysbin (pure paths) anywhere", () => {
    const roots = new Set(["src/cli.ts", "src/cli/real-deps.ts", "src/daemon-main.ts"]);
    const bad: string[] = [];
    for (const p of TS) {
      const rel = posix(relative(ROOT, join(ROOT, p)));
      if (rel.startsWith("src/platform/windows/") || roots.has(rel)) continue;
      for (const m of read(p).matchAll(/from "([^"]*platform\/windows\/[^"]*)"/g)) {
        if (!/platform\/windows\/sysbin(\.ts)?$/.test(m[1] as string)) bad.push(`${rel}: ${m[1]}`);
      }
    }
    expect(bad).toEqual([]);
  });

  test("the TUI and the view never import the daemon", () => {
    const bad = TS.filter((p) => /^src\/(tui|view)\//.test(posix(p))).filter((p) =>
      /from "\.\.\/daemon\//.test(read(p)),
    );
    expect(bad).toEqual([]);
  });
});
