// A separate process for test/mail/concurrency.test.ts: waits for the go file, then claims mail,
// answers a decision or asks one against the shared hub.db, and prints its result as JSON.
import { existsSync } from "node:fs";
import { parseRolesConfig } from "../../src/mail/roles";
import { answerAndNotify } from "../../src/mail/service";
import { Store } from "../../src/store/db";

const [dbPath, goFile, mode, arg] = process.argv.slice(2) as [string, string, string, string];
while (!existsSync(goFile)) await Bun.sleep(5);
const store = Store.open(dbPath, { create: false, migrate: false });
try {
  if (mode === "claim") {
    const got: number[] = [];
    let empty = 0;
    while (empty < 20) {
      const rows = store.mail().claim("beta", arg, Date.now());
      if (rows.length === 0) empty += 1;
      else got.push(...rows.map((m) => m.id));
      await Bun.sleep(1);
    }
    console.log(JSON.stringify(got));
  } else if (mode === "answer") {
    const r = answerAndNotify(store, parseRolesConfig(null), "D-0001", arg, "worker", Date.now());
    console.log(JSON.stringify(r.result));
  } else {
    const d = store.mail().createDecision({
      role: "alpha",
      question: arg,
      options: [],
      recommendation: null,
      fromSession: null,
      createdAt: Date.now(),
    });
    console.log(JSON.stringify(d.id));
  }
} finally {
  store.close();
}
