import { Box, Text } from "ink";
import { useEffect, useRef, useState } from "react";
import type { SessionView } from "../store/db";
import { ago, hhmm, profileHeader, wrap } from "./format";
import type { Register } from "./keys";
import { cleanText } from "./sanitize";

export interface HubMeta {
  unknownCount: number;
  unknownVersion: string | null;
  /** Profiles whose live list could not be refreshed, with the time it went stale (epoch ms). */
  stale: { profile: string; since: number }[];
  /** Relay messages that landed in a session other than the one they were sent to (verify.ts). */
  relayStrayCount: number;
}

export interface Line {
  text: string;
  color?: string;
  bold?: boolean;
  dim?: boolean;
}

/** Pure: the Status pane's lines for one session, wrapped to `width`. */
export function statusLines(s: SessionView, meta: HubMeta, now: number, width: number): Line[] {
  const out: Line[] = [];
  const add = (text: string, style: Omit<Line, "text"> = {}, indent = "") => {
    for (const t of wrap(text, width, indent)) out.push({ text: t, ...style });
  };
  for (const st of meta.stale) {
    add(`stale: ${profileHeader(st.profile)} list not updated for ${ago(now - st.since)}`, {
      color: "red",
    });
  }
  if (meta.unknownCount > 0) {
    const v = meta.unknownVersion ? ` (version ${meta.unknownVersion})` : "";
    add(`format changed: ${meta.unknownCount} unknown records skipped${v}`, { color: "red" });
  }
  if (meta.relayStrayCount > 0) {
    const n = meta.relayStrayCount;
    add(`relay: ${n} message${n === 1 ? "" : "s"} reached the wrong session (see run\\relay.log)`, {
      color: "red",
    });
  }
  const st = s.status;
  const lr = s.latestReply;
  if (st === null) {
    add("no status reply yet", { dim: true });
    return out;
  }
  if (lr && !lr.hasStatus && lr.replyUuid !== st.replyUuid && lr.replyTs > st.replyTs) {
    add(`newer reply at ${hhmm(lr.replyTs)} has no status block`, { color: "yellow" });
  }
  const age = Date.parse(st.replyTs);
  add(
    Number.isNaN(age)
      ? "status age unknown"
      : `status from ${ago(now - age)} ago (reply at ${hhmm(st.replyTs)} local)`,
    { dim: true },
  );
  if (st.progressPct !== null || st.phase !== null) {
    const pct = st.progressPct !== null ? `${st.progressPct}% overall` : "progress ?";
    add(`Progress: ${pct}. Phase: ${st.phase ?? "?"}`, { bold: true });
  }
  out.push({ text: "" });
  add("What's remaining", { bold: true });
  if (st.remaining.length === 0) add("  nothing", { dim: true });
  st.remaining.forEach((r, i) => {
    add(`  ${i + 1}. ${r}`, {}, "     ");
  });
  out.push({ text: "" });
  add("Waiting on you", { bold: true });
  if (st.waiting.length === 0) add("  nothing", { dim: true });
  for (const w of st.waiting) {
    const id = w.id ? `${w.id} ` : "";
    // An item the owner already answered after this list was posted stays visible, dimmed.
    if (w.answered) {
      add(`  ${id}(answered) ${w.text}`, { dim: true }, "    ");
      for (const c of w.children) add(`    - ${c}`, { dim: true }, "      ");
      continue;
    }
    const isNew = w.isNew ? "NEW " : "";
    add(`  ${id}${isNew}${w.text}`, w.isNew ? { color: "yellow" } : {}, "    ");
    for (const c of w.children) add(`    - ${c}`, { dim: true }, "      ");
  }
  if (st.recommendations.length > 0) {
    out.push({ text: "" });
    add("Recommendations", { bold: true });
    for (const r of st.recommendations) add(`  * ${r}`, {}, "    ");
  }
  return out;
}

/** Renders one line; cleanText here is a last guard, store text is already cleaned on read. */
export function LineView(props: { line: Line }) {
  const l = props.line;
  return (
    <Text wrap="truncate-end" color={l.color} bold={l.bold} dimColor={l.dim}>
      {l.text === "" ? " " : cleanText(l.text)}
    </Text>
  );
}

export function StatusPane(props: {
  session: SessionView;
  meta: HubMeta;
  now: number;
  width: number;
  height: number;
  register: Register;
}) {
  // The parent keys this pane by session, so a different session starts at the top.
  const [offset, setOffsetState] = useState(0);
  const offsetRef = useRef(0);
  const setOffset = (n: number) => {
    offsetRef.current = n;
    setOffsetState(n);
  };
  const lines = statusLines(props.session, props.meta, props.now, props.width);
  const h = Math.max(1, props.height);
  const maxOffset = Math.max(0, lines.length - h);
  const off = Math.min(offset, maxOffset);
  const { register } = props;
  const sid = props.session.sessionId;
  useEffect(() => {
    register({
      sessionId: sid,
      captures: () => false,
      handle: (_input, key) => {
        const cur = Math.min(offsetRef.current, maxOffset);
        if (key.pageDown) setOffset(Math.min(maxOffset, cur + h - 1));
        if (key.pageUp) setOffset(Math.max(0, cur - (h - 1)));
      },
    });
  });
  useEffect(() => () => register(null), [register]);
  const more = lines.length - off - h;
  const shown = more > 0 ? lines.slice(off, off + h - 1) : lines.slice(off, off + h);
  return (
    <Box flexDirection="column" width={props.width} height={h} overflow="hidden">
      {shown.map((l, i) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: lines are positional
        <LineView key={i} line={l} />
      ))}
      {more > 0 ? <Text dimColor>{`... ${more + 1} more lines (PgDn)`}</Text> : null}
    </Box>
  );
}
