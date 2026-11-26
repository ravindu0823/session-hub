import { describe, expect, test } from "bun:test";
import {
  DEFAULT_ACK_TIMEOUT_MS,
  DEFAULT_EXPIRY_MS,
  isRoleName,
  knownRoles,
  matchesSessionPattern,
  parseRolesConfig,
  RolesConfigError,
  roleOfSession,
} from "../../src/mail/roles";

describe("isRoleName", () => {
  test("accepts lower-case words with digits, dots, underscores and dashes", () => {
    for (const r of ["alpha", "web-api", "a1", "x.y_z", "a".repeat(64)]) {
      expect(isRoleName(r)).toBe(true);
    }
  });

  test("rejects upper case, spaces, leading punctuation, empty and over 64 characters", () => {
    for (const r of ["Alpha", "two words", "-x", ".x", "", "a".repeat(65), "a/b", 'a"b']) {
      expect(isRoleName(r)).toBe(false);
    }
  });
});

describe("matchesSessionPattern", () => {
  test("matches the whole name without case", () => {
    expect(matchesSessionPattern("alpha", "Alpha")).toBe(true);
    expect(matchesSessionPattern("alpha", "alpha-2")).toBe(false);
  });

  test("a star matches any run of characters, including none", () => {
    expect(matchesSessionPattern("alpha-*", "alpha-2")).toBe(true);
    expect(matchesSessionPattern("alpha*", "alpha")).toBe(true);
    expect(matchesSessionPattern("*-ops", "web-ops")).toBe(true);
    expect(matchesSessionPattern("alpha-*", "beta-2")).toBe(false);
  });

  test("regex characters in a pattern are literal", () => {
    expect(matchesSessionPattern("a.b", "a.b")).toBe(true);
    expect(matchesSessionPattern("a.b", "axb")).toBe(false);
    expect(matchesSessionPattern("a+", "aa")).toBe(false);
  });
});

describe("parseRolesConfig", () => {
  test("a missing file gives no roles and the default timings", () => {
    const cfg = parseRolesConfig(null);
    expect(cfg.roles).toEqual([]);
    expect(cfg.expiryMs).toBe(DEFAULT_EXPIRY_MS);
    expect(cfg.ackTimeoutMs).toBe(DEFAULT_ACK_TIMEOUT_MS);
  });

  test("reads roles, profiles, session patterns and timings", () => {
    const cfg = parseRolesConfig(
      JSON.stringify({
        roles: [{ role: "lead", profile: "work", sessions: ["lead", "boss-*"] }, { role: "web" }],
        mail: { expiryHours: 2, ackTimeoutMinutes: 5 },
      }),
    );
    expect(cfg.roles).toEqual([
      { role: "lead", profile: "work", sessions: ["lead", "boss-*"] },
      { role: "web", profile: null, sessions: ["web"] },
    ]);
    expect(cfg.expiryMs).toBe(2 * 3_600_000);
    expect(cfg.ackTimeoutMs).toBe(5 * 60_000);
  });

  test("a UTF-8 byte order mark is ignored", () => {
    expect(parseRolesConfig(`﻿{"roles":[{"role":"web"}]}`).roles).toHaveLength(1);
  });

  test("refuses invalid JSON, bad role names, reserved roles and duplicates", () => {
    const bad = [
      "{",
      "[]",
      JSON.stringify({ roles: [{ role: "Bad Name" }] }),
      JSON.stringify({ roles: [{ role: "owner" }] }),
      JSON.stringify({ roles: [{ role: "hub" }] }),
      JSON.stringify({ roles: [{ role: "web" }, { role: "web" }] }),
      JSON.stringify({ roles: [{ role: "web", sessions: [] }] }),
      JSON.stringify({ roles: [{ role: "web", sessions: [""] }] }),
      JSON.stringify({ roles: [{ role: "web", profile: 3 }] }),
      JSON.stringify({ roles: "web" }),
      JSON.stringify({ mail: { expiryHours: 0 } }),
      JSON.stringify({ mail: { ackTimeoutMinutes: -1 } }),
      JSON.stringify({ mail: { expiryHours: 10_000 } }),
    ];
    for (const text of bad) expect(() => parseRolesConfig(text)).toThrow(RolesConfigError);
  });
});

describe("roleOfSession", () => {
  const cfg = parseRolesConfig(
    JSON.stringify({
      roles: [
        { role: "lead", profile: "work", sessions: ["lead", "boss-*"] },
        { role: "games", profile: "home" },
      ],
    }),
  );

  test("a configured pattern in the right profile gives that role", () => {
    expect(roleOfSession(cfg, { name: "boss-7", profile: "work" })).toEqual({ role: "lead" });
  });

  test("a configured pattern in another profile does not match", () => {
    expect(roleOfSession(cfg, { name: "boss-7", profile: "home" })).toEqual({ role: "boss-7" });
  });

  test("with no configured match the session's own name is its role, lower-cased", () => {
    expect(roleOfSession(cfg, { name: "Web-API", profile: "work" })).toEqual({ role: "web-api" });
  });

  test("a name that is not a role name gives an error", () => {
    const r = roleOfSession(cfg, { name: "two words", profile: "work" });
    expect("error" in r).toBe(true);
  });

  test("a name that is another profile's configured role gives an error, not that role", () => {
    const r = roleOfSession(cfg, { name: "games", profile: "work" });
    expect("error" in r).toBe(true);
  });

  test("a reserved name gives an error", () => {
    expect("error" in roleOfSession(cfg, { name: "owner", profile: "work" })).toBe(true);
    expect("error" in roleOfSession(cfg, { name: "hub", profile: "work" })).toBe(true);
  });

  test("two configured roles matching one session is an error", () => {
    const two = parseRolesConfig(
      JSON.stringify({
        roles: [
          { role: "a", sessions: ["x*"] },
          { role: "b", sessions: ["*y"] },
        ],
      }),
    );
    expect("error" in roleOfSession(two, { name: "xy", profile: "work" })).toBe(true);
  });
});

describe("knownRoles", () => {
  test("configured roles, roles of indexed sessions and the owner", () => {
    const cfg = parseRolesConfig(JSON.stringify({ roles: [{ role: "lead" }] }));
    const roles = knownRoles(cfg, [
      { name: "web", profile: "work" },
      { name: "two words", profile: "work" },
    ]);
    expect([...roles].sort()).toEqual(["lead", "owner", "web"]);
  });
});
