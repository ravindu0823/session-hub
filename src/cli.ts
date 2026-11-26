import { Box, render, Text } from "ink";
import { createElement } from "react";
import { runCli } from "./cli/commands.ts";
import { realCliDeps, rolesForView, runRealBridge } from "./cli/real-deps.ts";
import { PROFILES, RELAY_LOG } from "./config.ts";
import { createLogger } from "./daemon/log.ts";
import { realClaudeCli } from "./discovery/agents.ts";
import { createWindowsPlatform } from "./platform/windows/index.ts";
import { send } from "./send/relay.ts";
import { buildMessage } from "./send/verify.ts";
import { Store } from "./store/db.ts";
import { App } from "./tui/App.tsx";

// hub.exe: every command, including the TUI. The daemon itself runs from hubd.exe (src/daemon-main.ts).

const now = () => Date.now();

const deps = realCliDeps({
  entryPath: import.meta.path,
  runBridge: (link) =>
    runRealBridge(link, { paths: deps.paths, version: deps.version, guardArgv: deps.guardArgv }),
  runTui: async ({ warning }) => {
    const { paths } = deps;
    const platform = createWindowsPlatform();
    const store = Store.open(paths.dbPath);
    try {
      const cli = realClaudeCli();
      const app = createElement(App, {
        store,
        send: (req: { sessionId: string; text: string; override?: boolean }) =>
          send(req, {
            store,
            cli,
            profiles: PROFILES,
            relayDir: paths.relayDir,
            now,
            log: createLogger(paths.runDir, undefined, RELAY_LOG),
            guardArgv: deps.guardArgv,
          }),
        focus: (pid: number) => platform.focusSessionWindow(pid),
        buildMessage,
        now,
        roles: () => rolesForView(paths.dataDir),
      });
      const root = warning
        ? createElement(
            Box,
            { flexDirection: "column" },
            app,
            createElement(Text, { color: "yellow" }, `! ${warning}`),
          )
        : app;
      const ink = render(root);
      await ink.waitUntilExit();
    } finally {
      store.close();
    }
  },
});

process.exit(await runCli(Bun.argv.slice(2), deps));
