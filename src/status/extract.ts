export interface WaitingItem {
  id: string | null;
  text: string;
  isNew: boolean;
  children: string[];
}

export interface Status {
  progressPct: number | null;
  phase: string | null;
  remaining: string[];
  waiting: WaitingItem[];
  recommendations: string[];
}

// Transcript text is untrusted and any size: a capture of "the rest of the line" is
// "([\s\S]*)" with no "$" after it, so it can never fail and backtrack over a whitespace run
// (".*$" fails on a "\r" or a line separator and retries every split of the run before it).
const PROGRESS_RE =
  /^\s*\*{0,2}Progress:\s*(?:about\s+)?(\d{1,3}(?:\.\d+)?)%\s+overall\.\s+Phase:(\s*)([\s\S]*)/i;
// The tail "\s*(?:#+\s*)?$" fails in linear time on a long whitespace run (no two adjacent \s*).
const REMAINING_HASH = /^\s{0,3}#{2,4}\s*\**what['\u2019]s remaining:?\**:?\s*(?:#+\s*)?$/i;
/** "## Waiting on you", optionally with a count: "## Waiting on you \u2014 5 items". */
const WAITING_HASH =
  /^\s{0,3}#{2,4}\s*\**waiting on you:?\**:?\s*(?:#+\s*|[\u2013\u2014-]\s*\d+\s+items?\b[\s\S]*)?$/i;
const REMAINING_BOLD = /^\s*\*\*what['\u2019]s remaining:?\*\*:?\s*([\s\S]*)/i;
const WAITING_BOLD = /^\s*\*\*waiting on you:?\*\*:?\s*([\s\S]*)/i;
const ANY_HEADING = /^\s{0,3}#{1,6}(\s|$)/;
const THEMATIC_BREAK = /^\s*(?:-{3,}|\*{3,}|_{3,})\s*$/;
const TABLE_ROW = /^\s*\|/;
const BULLET_RE = /^(\s*)(?:\d+[.)]|[-*\u2022])\s+([\s\S]*)/;
const NEW_RE = /^(\*{0,2})New:(\*{0,2})\s*/i;
/**
 * A question ID: segments of an upper-case letter and up to 7 upper-case letters or digits,
 * joined by single "-", ending in a digit (Q14, AUTH-Q1, AB2-Q3), followed by ":", whitespace,
 * ",", a dash or the end. At most MAX_ID_CHARS long.
 */
const ID_RE = /^((?:[A-Z][A-Z0-9]{0,7}-)*[A-Z][A-Z0-9]{0,6}\d)(?=[:\s,\u2013\u2014-]|$)/;
const MAX_ID_CHARS = 24;
const RANGE_RE = /^[\u2013\u2014-]\s*[A-Z][A-Z0-9]{0,7}(?:-[A-Z][A-Z0-9]{0,7})*\d/;
const RECOMMEND_RE = /recommend|my pick|\(recommended\)/i;
const MAX_RECOMMENDATIONS = 5;

// The standard "Waiting on you" template (the owner's global reply format): group headers, tables
// with the ID in a cell, 🆕 for new items, and a ✅ group of settled items. Tables are read only
// inside a group; outside one, a table ends the section as it always did. Patterns are anchored
// and bounded; the 🆕 mark is removed by string split, not by a regex over whitespace.

/** The standard template's "new" mark (🆕). */
const NEW_MARK = "\u{1F195}";
/** A cell longer than this is never an ID. */
const MAX_ID_CELL = 64;
/** A header label longer than this is not repeated on each row (it would multiply the output). */
const MAX_LABEL = 40;
/** Rows read from one table; the rest are ignored (bounds the output of one table). */
const MAX_TABLE_ROWS = 200;
const GROUP_EMOJI = "(\u{1F534}|\u{1F7E1}|\u{1F7E2}|⚪|✅)️?";
/**
 * "🔴 **Urgent**" or "**🔴 Urgent**", at indent 0 only: a mark inside an item is never one. A
 * markdown heading with a mark ("#### 🔴 Urgent") is a heading and ends the section.
 */
const GROUP_BOLD = new RegExp(`^(?:\\*\\*${GROUP_EMOJI}|${GROUP_EMOJI} ?\\*\\*)`, "u");
const SETTLED_MARK = "✅";

/**
 * The group mark when the line is a group header, else null. A mark line whose first bold span is
 * exactly an ID ("✅ **SH2** answered") is an item line, as it was before the template existed;
 * "🔴 **P0 blockers**" is a group.
 */
function groupMark(line: string): string | null {
  const m = GROUP_BOLD.exec(line);
  if (!m) return null;
  const open = line.indexOf("**");
  const close = line.indexOf("**", open + 2);
  if (close !== -1 && close - open <= MAX_ID_CELL) {
    const span = stripMarkdown(withoutNewMark(line.slice(open + 2, close))).trim();
    if (span !== "" && idAt(span) === span) return null;
  }
  return m[1] ?? m[2] ?? null;
}
/** "— 5 items (…)" alone after the Waiting on you heading: a count, not an item. */
const COUNT_RE = /^[–—-]\s*\d+\s+items?\b(?:\s*\([^)\n]*\))?\s*$/i;
/** "— 1 item: SH1 approve": the count before the colon is dropped, the item after it kept. */
const COUNT_PREFIX_RE = /^[–—-]\s*\d+\s+items?\b[^:\n]*:\s*/i;
/**
 * Markers the section loop puts between lines, out of band (objects, never strings, so no
 * transcript text can forge one): a group start (a fresh list) and a skip to the next group.
 */
interface Marker {
  mark: "group" | "skip";
}
const GROUP_START: Marker = { mark: "group" };
const SKIP_TO_GROUP: Marker = { mark: "skip" };
type SectionLine = string | Marker;

/** The text without 🆕 and the whitespace before each one; linear in the text. */
function withoutNewMark(s: string): string {
  if (!s.includes(NEW_MARK)) return s;
  const parts = s.split(NEW_MARK);
  return parts.map((p, i) => (i < parts.length - 1 ? p.trimEnd() : p)).join("");
}

const isSeparator = (cells: string[]): boolean => cells.every((c) => /^:?-{3,}:?$/.test(c));

/** The cells of a table row; a "|" inside a code span or escaped as "\|" does not split. */
function tableCells(line: string): string[] {
  const cells: string[] = [];
  let cell = "";
  let inCode = false;
  let s = line.trim();
  if (s.startsWith("|")) s = s.slice(1);
  if (s.endsWith("|") && !s.endsWith("\\|")) s = s.slice(0, -1);
  for (let i = 0; i < s.length; i++) {
    const c = s[i] as string;
    if (c === "\\" && s[i + 1] === "|") {
      cell += "|";
      i++;
    } else if (c === "`") {
      inCode = !inCode;
      cell += c;
    } else if (c === "|" && !inCode) {
      cells.push(cell.trim());
      cell = "";
    } else {
      cell += c;
    }
  }
  cells.push(cell.trim());
  return cells;
}

function isIdCell(c: string): boolean {
  if (c.length > MAX_ID_CELL) return false;
  const plain = stripMarkdown(withoutNewMark(c));
  return plain !== "" && idAt(plain) === plain;
}

/** A table's header row, prepared once: plain labels and the column headed "ID" (or -1). */
interface TableHead {
  labels: string[];
  idCol: number;
}

function tableHead(cells: string[]): TableHead {
  const labels = cells.map((c) => {
    const l = stripMarkdown(c.length > MAX_LABEL * 4 ? "" : c);
    return l.length > MAX_LABEL ? "" : l;
  });
  return { labels, idCol: labels.findIndex((l) => l.toLowerCase() === "id") };
}

/**
 * One table row as an item line: the ID cell first (the column headed "ID" when there is one,
 * else the first cell that is exactly an ID), then the other cells, each labelled with its column
 * header unless the column is the "What" one. Null for a row with no text at all. Linear in the
 * row: the header was prepared once (tableHead).
 */
function rowItem(cells: string[], head: TableHead | null): { line: string; hasId: boolean } | null {
  const idCol = head?.idCol ?? -1;
  const at = idCol !== -1 && isIdCell(cells[idCol] ?? "") ? idCol : cells.findIndex(isIdCell);
  const parts: { text: string; what: boolean }[] = [];
  cells.forEach((c, i) => {
    if (i === at || c === "") return;
    const h = head?.labels[i] ?? "";
    const what = h === "" || /^what\b/i.test(h);
    parts.push({ text: what ? c : `${h}: ${c}`, what });
  });
  parts.sort((a, b) => Number(b.what) - Number(a.what));
  const body = parts.map((p) => p.text).join(" · ");
  if (at === -1) return body === "" ? null : { line: body, hasId: false };
  const id = cells[at] as string;
  return { line: body === "" ? id : `${id} · ${body}`, hasId: true };
}

type Kind = "remaining" | "waiting";

interface Heading {
  kind: Kind;
  rest: string;
}

function stripMarkdown(s: string): string {
  const codes: string[] = [];
  const mark = "";
  const holder = /\uE000(\d+)\uE000/g;
  const masked = s.replace(/`([^`\n]*)`/g, (_m, code: string) => {
    codes.push(code);
    return `${mark}${codes.length - 1}${mark}`;
  });
  return masked
    .replace(/\*{1,3}(?=\S)([^*\n]*?\S)\*{1,3}/g, "$1")
    .replace(/(^|\W)(_{1,2})(?=\S)([^_\n]*?\S)\2(?!\w)/g, "$1$3")
    .replace(/\s+/g, " ")
    .trim()
    .replace(holder, (_m, i: string) => codes[Number(i)] ?? "");
}

function matchHeading(line: string): Heading | null {
  if (REMAINING_HASH.test(line)) return { kind: "remaining", rest: "" };
  if (WAITING_HASH.test(line)) return { kind: "waiting", rest: "" };
  const r = REMAINING_BOLD.exec(line);
  if (r) return { kind: "remaining", rest: (r[1] ?? "").trim() };
  const w = WAITING_BOLD.exec(line);
  if (w) return { kind: "waiting", rest: (w[1] ?? "").trim() };
  return null;
}

/** Lines outside ``` fences. */
function unfenced(markdown: string): string[] {
  const out: string[] = [];
  let inFence = false;
  for (const line of markdown.split(/\r?\n/)) {
    if (/^\s*```/.test(line)) {
      inFence = !inFence;
      continue;
    }
    if (!inFence) out.push(line);
  }
  return out;
}

interface RawItem {
  text: string;
  children: string[];
}

function indentOf(ws: string): number {
  return ws.replace(/\t/g, "    ").length;
}

function parseItems(lines: SectionLine[]): RawItem[] {
  const items: RawItem[] = [];
  const hasBullets = lines.some((l) => typeof l === "string" && BULLET_RE.test(l));
  let topIndent: number | null = null;
  let lastIsChild = false;
  let lastIndent = 0;
  let blankSeen = false;
  // After a group header of the standard template: a fresh list, so lines before its first
  // bullet are a lead-in (dropped), and a blank line before the header does not end the list.
  let leadIn = false;
  let inGroups = false;
  /** Whether the group starting at each GROUP_START line has a bullet before the next one. */
  const groupHasBullets = new Map<number, boolean>();
  let seen = false;
  for (let i = lines.length - 1; i >= 0; i--) {
    const l = lines[i];
    if (l === GROUP_START) {
      groupHasBullets.set(i, seen);
      seen = false;
    } else if (typeof l === "string" && BULLET_RE.test(l)) {
      seen = true;
    }
  }
  let groupBullets = hasBullets;
  /** Where main stopped at the end of the list: skip to the next group, if there is one. */
  let skipToGroup = false;
  for (const [i, line] of lines.entries()) {
    if (line === GROUP_START) {
      blankSeen = false;
      leadIn = true;
      inGroups = true;
      skipToGroup = false;
      groupBullets = groupHasBullets.get(i) ?? false;
      continue;
    }
    if (line === SKIP_TO_GROUP) {
      skipToGroup = true;
      continue;
    }
    if (skipToGroup || typeof line !== "string") continue;
    if (line.trim() === "") {
      blankSeen = true;
      continue;
    }
    const m = BULLET_RE.exec(line);
    if (m) {
      leadIn = false;
      const indent = indentOf(m[1] ?? "");
      const body = (m[2] ?? "").trim();
      if (topIndent === null) topIndent = indent;
      const last = items[items.length - 1];
      if (indent <= topIndent || !last) {
        items.push({ text: body, children: [] });
        lastIsChild = false;
      } else {
        last.children.push(body);
        lastIsChild = true;
      }
      lastIndent = indent;
      blankSeen = false;
      continue;
    }
    const indent = indentOf(/^\s*/.exec(line)?.[0] ?? "");
    if (leadIn) {
      // The first plain line of a group starts a new item. Where the group uses bullets, only
      // one that opens with an ID does; any other is a lead-in and is dropped.
      if (groupBullets && idAt(stripMarkdown(line)) === null) continue;
      leadIn = false;
      items.push({ text: line.trim(), children: [] });
      lastIsChild = false;
      lastIndent = indent;
      blankSeen = false;
      continue;
    }
    const last = items[items.length - 1];
    if (!last) {
      if (hasBullets) continue;
      items.push({ text: line.trim(), children: [] });
      lastIsChild = false;
      lastIndent = indent;
      blankSeen = false;
      continue;
    }
    if (blankSeen && indent <= lastIndent) {
      // In a group, a plain line that opens with an ID after a blank line is the next item.
      // Anything else ends the list as on main; a later group header starts a new one.
      if (inGroups && idAt(stripMarkdown(line)) !== null) {
        items.push({ text: line.trim(), children: [] });
        lastIsChild = false;
        lastIndent = indent;
        blankSeen = false;
        continue;
      }
      skipToGroup = true;
      continue;
    }
    blankSeen = false;
    if (lastIsChild && last.children.length > 0) {
      const i = last.children.length - 1;
      last.children[i] = `${last.children[i]} ${line.trim()}`;
    } else {
      last.text = `${last.text} ${line.trim()}`;
    }
  }
  if (items.length === 1) {
    const only = items[0];
    if (only && only.children.length === 0 && /^nothing\.?$/i.test(stripMarkdown(only.text)))
      return [];
  }
  return items;
}

const stem = (id: string): string => id.replace(/\d+$/, "");
/** Same stem, or the second's stem is the first's last segments ("AUTH-Q1-Q3": Q is the tail of AUTH-Q). */
const sameStem = (first: string, second: string): boolean =>
  stem(first) === stem(second) || stem(first).endsWith(`-${stem(second)}`);
const num = (id: string): number => Number(/\d+$/.exec(id)?.[0] ?? Number.NaN);

/**
 * The ID at the start of the text, or null. "Q1-Q3", "AUTH-Q1-AUTH-Q3" and "AUTH-Q1-Q3" are
 * ranges (the second ID has the first one's stem or its tail, and a higher number): the ID is the
 * first one. "AB2-Q3" is one ID.
 */
function idAt(text: string): string | null {
  const id = ID_RE.exec(text)?.[1];
  if (id === undefined || id.length > MAX_ID_CHARS) return null;
  for (let i = id.indexOf("-"); i !== -1; i = id.indexOf("-", i + 1)) {
    const first = id.slice(0, i);
    const second = id.slice(i + 1);
    if (/\d$/.test(first) && sameStem(first, second) && num(second) > num(first)) {
      return first;
    }
  }
  return id;
}

function toWaiting(raw: RawItem): WaitingItem {
  let text = raw.text;
  const nm = NEW_RE.exec(text);
  // The standard template marks a new item with 🆕 anywhere in it.
  const marked = text.includes(NEW_MARK);
  if (marked) text = withoutNewMark(text);
  const isNew = nm !== null || marked;
  if (nm) {
    // "**New: SH9** foo": the opening ** has no closing one in the prefix, keep it for the body.
    const reopen = nm[1] !== "" && nm[2] === "" ? nm[1] : "";
    text = reopen + text.slice(nm[0].length);
  }
  const children = raw.children.map(stripMarkdown);
  const full = stripMarkdown(text);
  // An item that opens with a code span is quoted code, never an ID.
  if (/^[*_\s]*`/.test(text)) return { id: null, text: full, isNew, children };
  const id = idAt(full);
  if (id === null) return { id: null, text: full, isNew, children };
  const rest = full.slice(id.length);
  if (RANGE_RE.test(rest)) return { id, text: full, isNew, children };
  const body = rest
    .replace(/^\s*:\s*/, "")
    .replace(/^\s*[,\u00b7\u2013\u2014-]\s+(?=\S)/, "")
    .replace(/^\s+/, "");
  return { id, text: body === "" ? full : body, isNew, children };
}

function toRemaining(raw: RawItem): string {
  const head = stripMarkdown(raw.text);
  const kids = raw.children.map(stripMarkdown).filter((c) => c !== "");
  if (kids.length === 0) return head;
  const sep = head.endsWith(":") ? " " : ": ";
  return `${head}${sep}${kids.join("; ")}`;
}

/** The last section of this kind that has items; empty when none do. */
function pickItems(sections: SectionLine[][] | undefined): RawItem[] {
  let picked: RawItem[] = [];
  for (const lines of sections ?? []) {
    const items = parseItems(lines);
    if (items.length > 0) picked = items;
  }
  return picked;
}

export function extractStatus(markdown: string): Status | null {
  const lines = unfenced(markdown);

  let progressPct: number | null = null;
  let phase: string | null = null;
  for (const line of lines) {
    const m = PROGRESS_RE.exec(line);
    // The phase ends at the first "**" after its first character (as the old lazy "(.+?)" did) or
    // at the end of the line, without a final ".": found by string search, not by a regex tail.
    // As on main, at least one character (a space counts) must follow "Phase:".
    const rest = m?.[3] ?? "";
    const bold = rest.indexOf("**", 1);
    let raw = bold === -1 ? rest.trimEnd() : rest.slice(0, bold);
    if (raw.length > 1 && raw.endsWith(".")) raw = raw.slice(0, -1);
    if (m?.[1] && (m[2] ?? "") + rest !== "") {
      progressPct = Math.min(100, Math.max(0, Math.round(Number(m[1]))));
      phase = stripMarkdown(raw);
      break;
    }
  }

  const sections: Record<Kind, SectionLine[][]> = { remaining: [], waiting: [] };
  let current: SectionLine[] | null = null;
  let kind: Kind | null = null;
  // The standard Waiting on you template: group headers, tables with the ID in a cell, and a
  // settled group that holds no open items. Tables are read only inside a group; outside one,
  // any table ends the section, exactly as before the template existed.
  let inGroup = false;
  let settled = false;
  /** A blank line was seen in the settled group. */
  let settledBlank = false;
  let head: TableHead | null = null;
  let pending: { cells: string[]; indent: number; at: number } | null = null;
  /** Lines read as table rows or settled items: never recommendations. */
  const consumed = new Set<number>();
  /** Rows kept from the current table; more than MAX_TABLE_ROWS are read but not kept. */
  let tableRows = 0;
  const addRow = (cells: string[], h: TableHead | null, indent: number, at: number) => {
    const row = rowItem(cells, h);
    if (row === null) return;
    consumed.add(at);
    if (tableRows >= MAX_TABLE_ROWS) return;
    tableRows++;
    (current as SectionLine[] | null)?.push(`${" ".repeat(indent)}- ${row.line}`);
  };
  /** A table ended: its first row, if no separator followed it, is a row too. */
  const closeTable = () => {
    const p = pending;
    pending = null;
    head = null;
    tableRows = 0;
    if (p !== null) addRow(p.cells, null, p.indent, p.at);
  };
  lines.forEach((line, at) => {
    const heading = matchHeading(line);
    if (heading) {
      closeTable();
      current = [];
      kind = heading.kind;
      inGroup = false;
      settled = false;
      sections[heading.kind].push(current);
      let rest = heading.rest;
      if (heading.kind === "waiting") {
        rest = COUNT_RE.test(rest) ? "" : rest.replace(COUNT_PREFIX_RE, "");
      }
      if (rest !== "") current.push(`- ${rest}`);
      return;
    }
    const inWaiting = current !== null && kind === "waiting";
    const mark = inWaiting ? groupMark(line) : null;
    if (inWaiting && settled && mark === null) {
      // The settled group runs to the next group, heading or break, or to a paragraph at the
      // margin after a blank line: text after the list, which ends the section.
      if (line.trim() === "") {
        settledBlank = true;
        consumed.add(at);
        return;
      }
      const paragraph =
        settledBlank && line === line.trimStart() && !BULLET_RE.test(line) && !TABLE_ROW.test(line);
      settledBlank = false;
      if (paragraph) {
        // Text after the settled list: the open list ends here, but a later group still counts.
        settled = false;
        (current as SectionLine[] | null)?.push(SKIP_TO_GROUP);
        return;
      }
      if (!ANY_HEADING.test(line) && !THEMATIC_BREAK.test(line)) {
        consumed.add(at);
        return;
      }
    }
    if (inWaiting && inGroup && !settled && TABLE_ROW.test(line)) {
      const ws = line.slice(0, line.length - line.trimStart().length);
      // A table indented like a list continuation (2 or more) belongs to the item above it.
      const indent = indentOf(ws) < 2 ? 0 : indentOf(ws);
      const cells = tableCells(line);
      if (isSeparator(cells)) {
        consumed.add(at);
        if (head !== null) return;
        if (pending !== null) consumed.add(pending.at);
        head = tableHead(pending?.cells ?? []);
        pending = null;
      } else if (head !== null) {
        // A row with no ID cell, followed by a separator, is the header of a new table right
        // under this one; a row with an ID stays a row (a stray separator in a body is ignored).
        const next = lines[at + 1];
        if (
          next !== undefined &&
          !cells.some(isIdCell) &&
          TABLE_ROW.test(next) &&
          isSeparator(tableCells(next))
        ) {
          head = null;
          tableRows = 0;
          pending = { cells, indent, at };
          return;
        }
        addRow(cells, head, indent, at);
      } else {
        const p = pending;
        if (p !== null) addRow(p.cells, null, p.indent, p.at);
        pending = { cells, indent, at };
      }
      return;
    }
    closeTable();
    if (mark !== null) {
      inGroup = true;
      settled = mark === SETTLED_MARK;
      settledBlank = false;
      consumed.add(at);
      if (!settled) (current as SectionLine[] | null)?.push(GROUP_START);
      return;
    }
    if (ANY_HEADING.test(line) || THEMATIC_BREAK.test(line) || TABLE_ROW.test(line)) {
      current = null;
      kind = null;
      return;
    }
    (current as SectionLine[] | null)?.push(line);
  });
  closeTable();

  const hasSection = sections.remaining.length > 0 || sections.waiting.length > 0;
  if (progressPct === null && !hasSection) return null;

  const recommendations: string[] = [];
  for (const [at, line] of lines.entries()) {
    if (consumed.has(at) || !RECOMMEND_RE.test(line)) continue;
    const text = stripMarkdown(line.replace(/^\s*(?:\d+[.)]|[-*\u2022]|#{1,6})\s+/, ""));
    if (text && !recommendations.includes(text)) recommendations.push(text);
    if (recommendations.length >= MAX_RECOMMENDATIONS) break;
  }

  return {
    progressPct,
    phase,
    remaining: pickItems(sections.remaining).map(toRemaining),
    waiting: pickItems(sections.waiting).map(toWaiting),
    recommendations,
  };
}
