import { randomUUID } from "node:crypto";
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";

const env = process.env;
const args = process.argv.slice(2);

/**
 * Writes all of `text` to stdout before returning. process.stdout.write is asynchronous on a Linux
 * pipe, so a process.exit right after it can cut large output short.
 */
async function out(text: string): Promise<void> {
  await Bun.write(Bun.stdout, text);
}

async function sleep(ms: number): Promise<void> {
  await new Promise((r) => setTimeout(r, ms));
}

if (args.includes("agents") && args.includes("--json")) {
  if (env.FAKE_AGENTS_MARKER) {
    await out(env.FAKE_AGENTS_MARKER);
    process.stderr.write(env.FAKE_AGENTS_MARKER);
  }
  if (env.FAKE_AGENTS_GRANDCHILD_PID_FILE) {
    const gc = Bun.spawn(["bun", "-e", "setTimeout(() => {}, 30000)"], {
      stdin: "ignore",
      stdout: "inherit",
      stderr: "ignore",
    });
    writeFileSync(env.FAKE_AGENTS_GRANDCHILD_PID_FILE, String(gc.pid));
  }
  if (env.FAKE_AGENTS_ECHO_CONFIG === "1") {
    await out(JSON.stringify([{ pid: 1, sessionId: "echo", cwd: env.CLAUDE_CONFIG_DIR ?? "" }]));
    process.exit(0);
  }
  const delay = Number(env.FAKE_AGENTS_DELAY_MS ?? 0);
  if (delay > 0) await sleep(delay);
  if (env.FAKE_AGENTS_JSON) await out(readFileSync(env.FAKE_AGENTS_JSON, "utf8"));
  process.exit(Number(env.FAKE_AGENTS_EXIT ?? 0));
}

if (args.includes("-p")) {
  const stdin = await new Response(Bun.stdin.stream()).text();
  if (env.FAKE_RELAY_LOG) {
    const line = {
      argv: args,
      stdin,
      configDir: env.CLAUDE_CONFIG_DIR,
      envKeys: Object.keys(env),
      cwd: process.cwd(),
    };
    appendFileSync(env.FAKE_RELAY_LOG, `${JSON.stringify(line)}\n`);
  }
  if (env.FAKE_TARGET_TRANSCRIPT && env.FAKE_RELAY_NO_DELIVERY !== "1") {
    const record = {
      type: "attachment",
      isSidechain: false,
      isMeta: true,
      uuid: randomUUID(),
      timestamp: new Date().toISOString(),
      attachment: {
        type: "queued_command",
        commandMode: "prompt",
        origin: {
          kind: "peer",
          name: "session-hub-relay",
          fromMode: "bypass",
          body: stdin.trimEnd(),
          verifiedLocal: true,
        },
      },
    };
    appendFileSync(env.FAKE_TARGET_TRANSCRIPT, `${JSON.stringify(record)}\n`);
  }
  // FAKE_RELAY_TRACE=1 adds the usage fields `claude -p --output-format json` reports.
  const trace =
    env.FAKE_RELAY_TRACE === "1"
      ? {
          duration_ms: 4321,
          total_cost_usd: 0.0061,
          usage: { input_tokens: 1200, output_tokens: 34 },
          modelUsage: { "claude-haiku-fixture": { inputTokens: 1200 } },
        }
      : {};
  await out(
    `${JSON.stringify({ result: env.FAKE_RELAY_RESULT ?? "DONE", is_error: false, ...trace })}
`,
  );
  process.exit(Number(env.FAKE_RELAY_EXIT ?? 0));
}

process.exit(2);
