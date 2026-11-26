import { Box, Text } from "ink";
import type { MailSummary } from "../view/snapshot";
import { wrap } from "./format";
import type { Line } from "./StatusPane";

/**
 * Pure: the Decisions tab's lines, wrapped to `width`. Every string in `mail` was cleaned by
 * readSnapshot. Read-only: the owner answers in the tray or with `hub answer` outside Claude Code.
 */
export function decisionLines(mail: MailSummary, width: number): Line[] {
  const out: Line[] = [];
  const add = (text: string, style: Omit<Line, "text"> = {}, indent = "") => {
    for (const t of wrap(text, width, indent)) out.push({ text: t, ...style });
  };
  for (const r of mail.waitingRoles) {
    add(`start a session named ${r.role}: ${r.queued} queued`, { color: "yellow" });
  }
  if (mail.flagged > 0) {
    add(`${mail.flagged} delivered mail without an ack after the timeout (hub mail sent)`, {
      color: "yellow",
    });
  }
  if (mail.ownerOpen > 0) {
    add(`${mail.ownerOpen} open mail for you: hub mail inbox, outside Claude Code`, {
      color: "yellow",
    });
  }
  if (out.length > 0) out.push({ text: "" });
  if (mail.decisions.length === 0) {
    add("no decisions", { dim: true });
    return out;
  }
  for (const d of mail.decisions) {
    add(`${d.id}  ${d.state}  ${d.role}`, d.state === "open" ? { bold: true } : { dim: true });
    for (const q of d.question.split("\n")) add(q, {}, "  ");
    if (d.options.length > 0) add(`options: ${d.options.join(" | ")}`, {}, "  ");
    if (d.recommendation !== null) add(`recommended: ${d.recommendation}`, {}, "  ");
    if (d.answer !== null) add(`answer: ${d.answer} (${d.answeredVia ?? "?"})`, {}, "  ");
    if (d.conflicts > 0) {
      const what = d.conflicts === 1 ? "other answer" : "other answers";
      add(`${d.conflicts} ${what} not applied or replaced`, { color: "red" }, "  ");
    }
    if (d.state === "open") {
      add(`answer in the tray, or: hub answer ${d.id} --text "..."`, { dim: true }, "  ");
    }
    out.push({ text: "" });
  }
  return out;
}

export function DecisionsPane(props: { mail: MailSummary; width: number; height: number }) {
  const all = decisionLines(props.mail, props.width);
  const lines = all.length > props.height ? all.slice(0, Math.max(0, props.height - 1)) : all;
  const more = all.length - lines.length;
  return (
    <Box flexDirection="column" width={props.width} height={props.height}>
      {lines.map((l, i) => (
        <Text
          // biome-ignore lint/suspicious/noArrayIndexKey: static lines of one render
          key={i}
          wrap="truncate-end"
          {...(l.color === undefined ? {} : { color: l.color })}
          bold={l.bold ?? false}
          dimColor={l.dim ?? false}
        >
          {l.text === "" ? " " : l.text}
        </Text>
      ))}
      {more > 0 ? <Text dimColor>{`... ${more} more lines (hub decisions lists all)`}</Text> : null}
    </Box>
  );
}
