/**
 * Which "Waiting on you" IDs the owner has answered since the list was posted, from the owner's own
 * messages and Session Hub relay messages. Conservative on purpose: a wrongly answered item drops out
 * of every count, while a missed one stays visible and can be dismissed in the tray.
 *
 * An ID answers when it is a whole token on the list (case-sensitive, IDs are upper case) and the
 * line it is on does not end with "?", and it opens the line (after list markers, quotes or bold) or
 * follows a separator ("/", ",", ";", "&", "and") of an earlier answer on that line:
 * "AB7 skip it", "Answers:\nAB9: yes", "Q6 Yes / Q7 Yes". A range ("AB7-AB9", "XY-Q1-Q3") at such a
 * place answers every listed ID of that stem inside it.
 */

/** A token boundary: no letter, digit or "-" on either side ("XY-Q1" is not "Q1", "Q14" is not "Q1"). */
const ID_TOKEN = /(?<![A-Za-z0-9-])((?:[A-Z][A-Z0-9]{0,7}-)*[A-Z][A-Z0-9]{0,6}\d)(?![A-Za-z0-9])/g;
/** What may come before an answering ID: line start plus markers, or a separator after an answer. */
const LEAD = /^[\s>*_`(#\-•]*(?:\d+[.)]\s*)?$/;
const SEPARATOR = /(?:[/,;&]|\band)[\s*_`(]*$/;
const DASH = /^\s*[-–—]\s*$/;
/** Text scanned per message and longest token tried as a range: bounds the work on hostile input. */
const MAX_SCAN_CHARS = 20_000;
const MAX_TOKEN_CHARS = 40;

const stem = (id: string): string => id.replace(/\d+$/, "");
const num = (id: string): number => Number(/\d+$/.exec(id)?.[0] ?? Number.NaN);

/**
 * "XY-Q1-Q3" or "XY-Q1-XY-Q3" (one token, since "-" joins segments): the two ends when the second
 * has the first one's stem or its last segment and a higher number; else null.
 */
function splitJoinedRange(token: string): [string, string] | null {
  for (let i = token.indexOf("-"); i !== -1; i = token.indexOf("-", i + 1)) {
    const first = token.slice(0, i);
    let second = token.slice(i + 1);
    if (!/\d$/.test(first) || !/^[A-Z]/.test(second)) continue;
    if (stem(first).endsWith(`-${stem(second)}`)) second = stem(first) + String(num(second));
    if (stem(first) === stem(second) && num(second) > num(first)) return [first, second];
  }
  return null;
}

export function answeredIds(listed: readonly string[], texts: readonly string[]): Set<string> {
  const out = new Set<string>();
  if (listed.length === 0) return out;
  const onList = new Set(listed);
  const markRange = (from: string, to: string): void => {
    for (const id of listed) {
      if (stem(id) === stem(from) && num(id) >= num(from) && num(id) <= num(to)) out.add(id);
    }
  };
  for (const text of texts) {
    for (const line of text.slice(0, MAX_SCAN_CHARS).split(/\r?\n/)) {
      if (line.trimEnd().endsWith("?")) continue;
      const tokens = [...line.matchAll(ID_TOKEN)];
      let end = 0;
      let answering = false;
      for (let i = 0; i < tokens.length; i++) {
        const m = tokens[i] as RegExpMatchArray;
        const token = m[1] as string;
        const at = m.index ?? 0;
        const before = line.slice(end, at);
        end = at + token.length;
        // Only the first token of a line may stand at its start; later ones need a separator.
        answering = i === 0 ? LEAD.test(before) : answering && SEPARATOR.test(before);
        if (!answering) {
          if (i === 0) break;
          continue;
        }
        if (onList.has(token)) out.add(token);
        if (token.length <= MAX_TOKEN_CHARS) {
          const joined = splitJoinedRange(token);
          if (joined) markRange(joined[0], joined[1]);
          // "Q14-Q6", "AB7-Q7": not a range, but each part that is an ID is named.
          else for (const part of token.split(/(?<=\d)-/)) if (onList.has(part)) out.add(part);
        }
        // "AB7 - AB9" / "AB7–AB9": the next token, with only a dash between, closes a range.
        const next = tokens[i + 1];
        if (next?.index === undefined) continue;
        const other = next[1] as string;
        if (
          DASH.test(line.slice(end, next.index)) &&
          stem(other) === stem(token) &&
          num(other) > num(token)
        ) {
          markRange(token, other);
          end = next.index + other.length;
          i++;
        }
      }
    }
  }
  return out;
}
