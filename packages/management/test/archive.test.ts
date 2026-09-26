import { expect, test } from "bun:test";

import { type ArchiveLimits, scanArchive } from "../src/releases/archive.ts";
import { contractProblem } from "../src/releases/contract.ts";
import { keepLimit, verifyRelease } from "../src/releases/manifest.ts";
import { type ArchiveOptions, rawArchive, releaseArchive } from "./fixtures.ts";

const limits: ArchiveLimits = { maxExpandedBytes: 64 * 1024 * 1024, maxEntries: 1000 };

function verify(archive: ReturnType<typeof releaseArchive>, overrides: Partial<ArchiveLimits> = {}, id = "r1") {
  return verifyRelease(new Blob([archive.bytes]).stream(), { releaseId: id, ...archive }, { ...limits, ...overrides });
}

function broken(options: ArchiveOptions) {
  return verify(releaseArchive("r1", undefined, options));
}

test("accepts what chunk build writes", async () => {
  const text = await verify(releaseArchive("r1", [{ id: "lobby", sessions: ["default", "duel"] }]));
  await expect(JSON.parse(text).apps[0].id).toBe("lobby");
});

test("rejects malformed archives and manifests", async () => {
  await expect(broken({ badChecksum: true })).rejects.toThrow("checksum");
  await expect(verify(rawArchive([["release.json", '{"id":"r1","apps":[]}']]))).rejects.toThrow("unexpected fields");
  await expect(broken({ manifest: (manifest) => ({ ...manifest, version: 2 }) })).rejects.toThrow("version 3");
  await expect(broken({ manifest: (manifest) => ({ ...manifest, id: "r2" }) })).rejects.toThrow("names release r2");
  await expect(verify(releaseArchive("r1"), {}, "r9")).rejects.toThrow("names release r1");
});

test("rejects releases whose payloads are missing or altered", async () => {
  await expect(broken({ omit: (path) => path.startsWith("apps/") })).rejects.toThrow("missing apps/lobby/");
  await expect(broken({ omit: (path) => path === "source.mjs" })).rejects.toThrow("missing source.mjs");
  await expect(
    broken({
      manifest: (manifest) => ({
        ...manifest,
        assets: Object.fromEntries(Object.keys(manifest.assets as object).map((path) => [path, "0".repeat(64)])),
      }),
    }),
  ).rejects.toThrow("does not match its digest");
});

test("rejects backend metadata chunk build would not write", async () => {
  const notJson = { "backend.json": "not JSON", "contract.json": "not JSON" };
  await expect(broken({ replace: notJson })).rejects.toThrow("contract.json is not a JSON object");
  await expect(broken({ replace: { "contract.json": '{"contract_version":2}' } })).rejects.toThrow("runtime_profile");
  const backend = (fields: object) => ({
    "backend.json": JSON.stringify({
      contract_version: 2,
      runtime_profile: "transactional_v1",
      tables: {},
      functions: {},
      id: "r1",
      source: "export default {};\n",
      ...fields,
    }),
  });
  await expect(broken({ replace: backend({ id: "r2" }) })).rejects.toThrow('names release "r2"');
  await expect(broken({ replace: backend({ source: "other" }) })).rejects.toThrow("does not carry source.mjs");
  await expect(broken({ replace: backend({ functions: { f: {} } }) })).rejects.toThrow("does not match contract.json");
});

type Json = Record<string, unknown>;

/** The object or list at `path` in `value`, for mutations that break the contract's types. */
function at(value: unknown, ...path: (string | number)[]): Json {
  return path.reduce<Json>((node, key) => node[key] as Json, value as Json);
}
const list = (value: unknown, ...path: (string | number)[]) => at(value, ...path) as unknown as unknown[];
const range = <T>(count: number, make: (i: number) => [string, T]) =>
  Object.fromEntries(Array.from({ length: count }, (_, i) => make(i)));
const field = (schema: object, optional?: boolean) => (optional === undefined ? { schema } : { schema, optional });
const object = (fields: object = {}) => ({ type: "object", fields });
const arrays = (depth: number) => {
  let schema: object = { type: "integer" };
  for (let i = 0; i < depth; i++) schema = { type: "array", items: schema };
  return schema;
};
const nestedValue = (depth: number) => {
  let value: unknown = [];
  for (let i = 1; i < depth; i++) value = [value];
  return value;
};
const query = (fields: object, result: object) => ({
  kind: "query",
  visibility: "internal",
  export: "extra",
  arguments: object(fields),
  result,
});
const party = (c: Json, ...path: (string | number)[]) =>
  at(c, "domains", "commands", "scopes/lobby/commands/party", ...path);
const entry = (c: Json, id: string, ...path: string[]) => at(c, "destinations", "entries", id, ...path);
const hook = (c: Json, id: string) => at(c, "domains", "hooks", id);

/** A contract as chunk-build writes it, using every section including the optional contracts. */
function validContract(): Json {
  return {
    contract_version: 2,
    runtime_profile: "transactional_v1",
    tables: {
      players: {
        fields: {
          name: field({ type: "string" }),
          score: field({ type: "integer" }, false),
          rank: field({ type: "nullable", value: { type: "enum", values: ["gold", "silver"] } }, true),
          tags: field({ type: "array", items: { type: "literal", value: "vip" } }),
          state: field({
            type: "union",
            variants: { online: object({ since: field({ type: "number" }) }), offline: object() },
          }),
          profile: field(object({ _id: field({ type: "id", table: "players" }), owner: field({ type: "player" }) })),
          session: field({ type: "session" }),
          banned: field({ type: "boolean" }),
          note: field({ type: "null" }),
        },
        indexes: { by_name: ["name"], by_score_name: ["score", "name"] },
      },
      matches: { fields: { player: field({ type: "id", table: "players" }) } },
    },
    functions: {
      "players/get": {
        kind: "query",
        visibility: "public",
        export: "get",
        arguments: object({ id: field({ type: "id", table: "players" }) }),
        result: { type: "nullable", value: object({ name: field({ type: "string" }) }) },
      },
      "players/can_manage": { ...query({}, { type: "boolean" }), export: "canManage" },
      "players/suggest": {
        ...query(
          { input: field({ type: "string" }), cursor: field({ type: "integer" }) },
          { type: "array", items: { type: "string" } },
        ),
        export: "suggest",
      },
      "matches/start": {
        kind: "mutation",
        visibility: "public",
        export: "start",
        arguments: object(),
        result: { type: "id", table: "matches" },
      },
      "matches/report": {
        kind: "action",
        visibility: "internal",
        export: "report",
        arguments: object(),
        result: { type: "null" },
      },
    },
    session_methods: {
      version: 1,
      methods: [
        {
          app: "lobby",
          session: "default",
          name: "kick",
          arguments: object({ player: field({ type: "player" }) }),
          result: { type: "boolean" },
        },
      ],
    },
    session_configurations: {
      version: 1,
      configurations: [
        {
          app: "lobby",
          session: "default",
          configuration: object({
            mode: field({ type: "enum", values: ["solo", "duo"] }),
            max_players: field({ type: "integer" }, true),
          }),
        },
      ],
    },
    destinations: {
      version: 1,
      entries: {
        "apps/lobby/destinations/main": {
          destination: { key: "main", session_type: "lobby/default", machine_profile: "default" },
          overflow: "replicate",
          empty_timeout_seconds: 300,
          creation: { capacity: 16, configuration: { mode: "solo" } },
        },
        "shared/destinations/duel": {
          destination: { key: "duel", session_type: "lobby/default", machine_profile: "default" },
          overflow: "reject",
          empty_timeout_seconds: 60,
          creation: { capacity: 2, configuration: { mode: "duo", max_players: 2 } },
        },
        "apps/arena/destinations/main": {
          destination: { key: "main", session_type: "arena/default", machine_profile: "large-1" },
          overflow: "replicate",
          empty_timeout_seconds: 86_400,
        },
      },
    },
    domains: {
      version: 1,
      scopes: { "": { parent: null }, lobby: { parent: "" }, "lobby/arena": { parent: "lobby" } },
      apps: { lobby: "lobby", arena: "lobby/arena" },
      hooks: {
        "shared/domains/hooks/ping": { domain: "", event: "server.ping", export: "onPing" },
        "scopes/lobby/hooks/first": { domain: "lobby", event: "player.login", export: "loginFirst", order: 1 },
        "scopes/lobby/hooks/second": { domain: "lobby", event: "player.login", export: "loginSecond", order: 2 },
        "apps/lobby/app/hooks/enter": {
          domain: "lobby",
          event: "domain.enter",
          export: "onEnter",
          follow_player: true,
        },
      },
      commands: {
        "scopes/lobby/commands/party": {
          domain: "lobby",
          name: "party",
          aliases: ["p"],
          export: "party",
          permission: "players/can_manage",
          follow_player: false,
          routes: [
            { literals: [], arguments: [] },
            {
              literals: ["invite"],
              arguments: [{ name: "player", parser: "word", suggestions: { query: "players/suggest" } }],
            },
            { literals: ["size"], arguments: [{ name: "size", parser: "integer", min: 2, max: 8 }] },
            {
              literals: ["say"],
              arguments: [
                { name: "tone", parser: "string", suggestions: ["loud", "quiet"] },
                { name: "text", parser: "greedy" },
              ],
            },
          ],
        },
        "apps/arena/app/commands/leave": {
          domain: "lobby/arena",
          name: "leave",
          aliases: [],
          export: "leave",
          follow_player: true,
          routes: [{ literals: [], arguments: [] }],
        },
      },
    },
  };
}

/** One contract per rule chunk_contract enforces: a name, a mutation of the valid contract, and the problem. */
const invalidContracts: [string, (c: Json) => void, string][] = [
  // BackendMetadata and Deployment
  ["a missing field", (c) => delete c.tables, "is missing tables"],
  ["an unknown field", (c) => (c.extra = 1), "unexpected field extra"],
  ["another contract version", (c) => (c.contract_version = 3), "contract version 2"],
  ["an unknown runtime profile", (c) => (c.runtime_profile = "v2"), "unknown runtime_profile"],
  // Tables
  ["tables that are not a map", (c) => (c.tables = []), "no tables map"],
  [
    "too many tables",
    (c) =>
      Object.assign(
        at(c, "tables"),
        range(127, (i) => [`t${i}`, { fields: {} }]),
      ),
    "more than 128 tables",
  ],
  ["a reserved table name", (c) => (at(c, "tables").sqlite_stats = { fields: {} }), "invalid table sqlite_stats"],
  [
    "table names that differ by case",
    (c) => (at(c, "tables").Matches = { fields: {} }),
    "table names that differ only by case",
  ],
  [
    "tables over 1 MiB once serde spells out defaults",
    (c) => {
      at(c, "tables", "matches", "fields").pad = field({ type: "literal", value: "" });
      const size = JSON.stringify(c.tables).length;
      at(c, "tables", "matches", "fields", "pad", "schema").value = "x".repeat(1024 * 1024 - size);
    },
    "tables over 1 MiB",
  ],
  ["an unknown table field", (c) => (at(c, "tables", "matches").primary = "player"), "unexpected field primary"],
  [
    "an unknown field key",
    (c) => (at(c, "tables", "matches", "fields", "player").default = 1),
    "unexpected field default",
  ],
  ["a null optional", (c) => (at(c, "tables", "matches", "fields", "player").optional = null), "non-boolean optional"],
  [
    "too many table fields",
    (c) => (at(c, "tables", "matches").fields = range(65, (i) => [`f${i}`, field({ type: "string" })])),
    "more than 64 fields",
  ],
  [
    "a storage _id on a table",
    (c) => (at(c, "tables", "matches", "fields")._id = field({ type: "id", table: "matches" })),
    "invalid field name _id",
  ],
  [
    "an invalid field name",
    (c) => (at(c, "tables", "matches", "fields")["9lives"] = field({ type: "string" })),
    "invalid field name 9lives",
  ],
  [
    "field names that differ by case",
    (c) => (at(c, "tables", "matches", "fields").Player = field({ type: "string" })),
    "field names that differ only by case",
  ],
  [
    "too many indexes",
    (c) =>
      Object.assign(
        at(c, "tables", "players", "indexes"),
        range(15, (i) => [`i${i}`, ["name"]]),
      ),
    "more than 16 indexes",
  ],
  ["an invalid index name", (c) => (at(c, "tables", "players", "indexes")["_by"] = ["name"]), "invalid index _by"],
  ["an empty index", (c) => (at(c, "tables", "players", "indexes").by_name = []), "invalid index by_name"],
  [
    "an index over 8 fields",
    (c) => (at(c, "tables", "players", "indexes").by_name = Array(9).fill("name")),
    "invalid index by_name",
  ],
  [
    "an index repeating a field",
    (c) => (at(c, "tables", "players", "indexes").by_name = ["name", "name"]),
    "without distinct declared scalar",
  ],
  [
    "an index on an undeclared field",
    (c) => (at(c, "tables", "players", "indexes").by_name = ["missing"]),
    "without distinct declared scalar",
  ],
  [
    "an index on a non-scalar field",
    (c) => (at(c, "tables", "players", "indexes").by_name = ["rank"]),
    "without distinct declared scalar",
  ],
  [
    "index names that differ by case",
    (c) => (at(c, "tables", "players", "indexes").BY_NAME = ["name"]),
    "index names that differ only by case",
  ],
  // Schemas
  [
    "an untagged schema",
    (c) => (at(c, "tables", "matches", "fields", "player").schema = "string"),
    "schema without a type",
  ],
  [
    "an unknown schema type",
    (c) => (at(c, "tables", "matches", "fields", "player").schema = { type: "strng" }),
    "unknown schema type",
  ],
  [
    "an unknown key on a struct schema",
    (c) => (at(c, "tables", "players", "fields", "tags", "schema").max = 3),
    "invalid array schema",
  ],
  [
    "a literal without a value",
    (c) => delete at(c, "tables", "players", "fields", "tags", "schema", "items").value,
    "invalid literal schema",
  ],
  [
    "an id of an invalid table",
    (c) => (at(c, "tables", "matches", "fields", "player", "schema").table = "sqlite_x"),
    "invalid table",
  ],
  [
    "an empty enum",
    (c) => (at(c, "tables", "players", "fields", "rank", "schema", "value").values = []),
    "enum schema with invalid values",
  ],
  [
    "an enum over 64 values",
    (c) =>
      (at(c, "tables", "players", "fields", "rank", "schema", "value").values = Array.from(
        { length: 65 },
        (_, i) => `v${i}`,
      )),
    "enum schema with invalid values",
  ],
  [
    "a repeated enum value",
    (c) => (at(c, "tables", "players", "fields", "rank", "schema", "value").values = ["gold", "gold"]),
    "enum schema with invalid values",
  ],
  [
    "an invalid enum value",
    (c) => (at(c, "tables", "players", "fields", "rank", "schema", "value").values = ["gold-plated"]),
    "enum schema with invalid values",
  ],
  [
    "too many object fields",
    (c) =>
      (at(c, "tables", "players", "fields", "profile", "schema").fields = range(65, (i) => [
        `f${i}`,
        field({ type: "string" }),
      ])),
    "more than 64 fields",
  ],
  [
    "an optional storage _id",
    (c) => (at(c, "tables", "players", "fields", "profile", "schema", "fields", "_id").optional = true),
    "invalid field name _id",
  ],
  [
    "a storage _id that is not an id",
    (c) => (at(c, "tables", "players", "fields", "profile", "schema", "fields", "_id").schema = { type: "string" }),
    "invalid field name _id",
  ],
  [
    "an empty union",
    (c) => (at(c, "tables", "players", "fields", "state", "schema").variants = {}),
    "without 1 to 16 variants",
  ],
  [
    "a union over 16 variants",
    (c) => (at(c, "tables", "players", "fields", "state", "schema").variants = range(17, (i) => [`v${i}`, object()])),
    "without 1 to 16 variants",
  ],
  [
    "an invalid union variant name",
    (c) => (at(c, "tables", "players", "fields", "state", "schema", "variants")["9"] = object()),
    "invalid union variant name",
  ],
  [
    "a scalar union variant",
    (c) => (at(c, "tables", "players", "fields", "state", "schema", "variants").offline = { type: "string" }),
    "not an object without a type field",
  ],
  [
    "a union variant with a type field",
    (c) =>
      (at(c, "tables", "players", "fields", "state", "schema", "variants").offline = object({
        type: field({ type: "string" }),
      })),
    "not an object without a type field",
  ],
  [
    "a non-scalar literal",
    (c) => (at(c, "tables", "players", "fields", "tags", "schema", "items").value = ["vip"]),
    "non-scalar literal",
  ],
  [
    "an unsafe literal",
    (c) => (at(c, "tables", "players", "fields", "tags", "schema", "items").value = 2 ** 53),
    "safe range",
  ],
  // Functions
  ["functions that are not a map", (c) => (c.functions = []), "no functions map"],
  [
    "too many functions",
    (c) =>
      Object.assign(
        at(c, "functions"),
        range(252, (i) => [`f${i}`, { ...query({}, { type: "null" }), export: `f${i}` }]),
      ),
    "more than 256 functions",
  ],
  ["an unknown function field", (c) => (at(c, "functions", "matches/start").cached = true), "unexpected field cached"],
  ["an unknown function kind", (c) => (at(c, "functions", "matches/start").kind = "subscription"), "unknown kind"],
  ["an unknown visibility", (c) => (at(c, "functions", "matches/start").visibility = "private"), "unknown visibility"],
  [
    "an invalid function path",
    (c) => (at(c, "functions")["matches//end"] = query({}, { type: "null" })),
    "invalid function path",
  ],
  [
    "a function path over 256 bytes",
    (c) => (at(c, "functions")[`${"a".repeat(128)}/${"b".repeat(128)}`] = query({}, { type: "null" })),
    "invalid function path",
  ],
  [
    "function paths that differ by case",
    (c) => (at(c, "functions")["Matches/Start"] = query({}, { type: "null" })),
    "differ only by case",
  ],
  ["a repeated export", (c) => (at(c, "functions", "matches/report").export = "start"), "invalid or repeated export"],
  ["an invalid export", (c) => (at(c, "functions", "matches/report").export = "re-port"), "invalid or repeated export"],
  [
    "an invalid function schema",
    (c) => (at(c, "functions", "matches/report").result = { type: "union", variants: {} }),
    "without 1 to 16 variants",
  ],
  [
    "a function inside another's namespace",
    (c) => (at(c, "functions").matches = query({}, { type: "null" })),
    "inside another function's namespace",
  ],
  // Session methods
  ["session methods that are not a list", (c) => (at(c, "session_methods").methods = 42), "no methods list"],
  ["an unknown session methods field", (c) => (at(c, "session_methods").extra = 1), "unexpected field extra"],
  [
    "an unknown session method field",
    (c) => (at(c, "session_methods", "methods", 0).timeout = 1),
    "unexpected field timeout",
  ],
  [
    "another session methods version",
    (c) => (at(c, "session_methods").version = 2),
    "session_methods: is not version 1",
  ],
  [
    "too many session methods",
    (c) =>
      list(c, "session_methods", "methods").push(
        ...Array.from({ length: 256 }, (_, i) => ({ ...at(c, "session_methods", "methods", 0), name: `m${i}` })),
      ),
    "more than 256 methods",
  ],
  [
    "an invalid session method identity",
    (c) => (at(c, "session_methods", "methods", 0).name = "kick-now"),
    "invalid method identity",
  ],
  [
    "a repeated session method",
    (c) => list(c, "session_methods", "methods").push({ ...at(c, "session_methods", "methods", 0), name: "Kick" }),
    "declares method",
  ],
  [
    "non-object session method arguments",
    (c) => (at(c, "session_methods", "methods", 0).arguments = { type: "null" }),
    "non-object arguments",
  ],
  [
    "an invalid session method schema",
    (c) => (at(c, "session_methods", "methods", 0).result = { type: "union", variants: {} }),
    "invalid method lobby/default/kick",
  ],
  // Session configurations
  [
    "session configurations that are not a list",
    (c) => (at(c, "session_configurations").configurations = {}),
    "no configurations list",
  ],
  [
    "an unknown session configuration field",
    (c) => (at(c, "session_configurations", "configurations", 0).defaults = {}),
    "unexpected field defaults",
  ],
  [
    "another session configurations version",
    (c) => (at(c, "session_configurations").version = 2),
    "session_configurations: is not version 1",
  ],
  [
    "too many session configurations",
    (c) =>
      list(c, "session_configurations", "configurations").push(
        ...Array.from({ length: 256 }, (_, i) => ({ app: "other", session: `s${i}`, configuration: object() })),
      ),
    "more than 256 configurations",
  ],
  [
    "an invalid session configuration identity",
    (c) => (at(c, "session_configurations", "configurations", 0).session = "a/b"),
    "invalid configuration identity",
  ],
  [
    "a repeated session configuration",
    (c) =>
      list(c, "session_configurations", "configurations").push({
        app: "LOBBY",
        session: "default",
        configuration: object(),
      }),
    "declares configuration",
  ],
  [
    "a non-object session configuration",
    (c) => (at(c, "session_configurations", "configurations", 0).configuration = { type: "string" }),
    "non-object",
  ],
  [
    "an invalid session configuration schema",
    (c) =>
      (at(c, "session_configurations", "configurations", 0).configuration = object({
        x: field({ type: "enum", values: [] }),
      })),
    "enum schema with invalid values",
  ],
  [
    "session configurations over 2 MiB",
    (c) =>
      list(c, "session_configurations", "configurations").push({
        app: "other",
        session: "big",
        configuration: object({ pad: field({ type: "literal", value: "x".repeat(2 * 1024 * 1024) }, true) }),
      }),
    "is over 2 MiB",
  ],
  // Destinations
  ["an unknown destinations field", (c) => (at(c, "destinations").extra = 1), "unexpected field extra"],
  [
    "another destinations version",
    (c) => (at(c, "destinations").version = 2),
    "is not version 1 with 1 to 256 entries",
  ],
  ["no destinations", (c) => (at(c, "destinations").entries = {}), "is not version 1 with 1 to 256 entries"],
  [
    "too many destinations",
    (c) =>
      Object.assign(
        at(c, "destinations", "entries"),
        range(254, (i) => [
          `apps/arena/destinations/d${i}`,
          {
            ...entry(c, "apps/arena/destinations/main"),
            destination: { key: `d${i}`, session_type: "arena/default", machine_profile: "default" },
          },
        ]),
      ),
    "is not version 1 with 1 to 256 entries",
  ],
  [
    "an unknown destination policy field",
    (c) => (entry(c, "shared/destinations/duel").priority = 1),
    "unexpected field priority",
  ],
  [
    "an unknown destination field",
    (c) => (entry(c, "shared/destinations/duel", "destination").region = "eu"),
    "invalid destination: has an unexpected field region",
  ],
  ["an unknown overflow", (c) => (entry(c, "shared/destinations/duel").overflow = "queue"), "unknown overflow"],
  [
    "an unknown creation field",
    (c) => (entry(c, "shared/destinations/duel", "creation").warm = true),
    "invalid creation: has an unexpected field warm",
  ],
  [
    "no creation capacity",
    (c) => (entry(c, "shared/destinations/duel", "creation").capacity = 0),
    "creation capacity outside 1 to 128",
  ],
  [
    "too much creation capacity",
    (c) => (entry(c, "shared/destinations/duel", "creation").capacity = 129),
    "creation capacity outside 1 to 128",
  ],
  [
    "a non-object creation configuration",
    (c) => (entry(c, "shared/destinations/duel", "creation").configuration = "duo"),
    "not an object within 65536 bytes",
  ],
  [
    "a creation configuration over 64 KiB",
    (c) => (entry(c, "shared/destinations/duel", "creation", "configuration").mode = "x".repeat(65_536)),
    "not an object within 65536 bytes",
  ],
  [
    "an unsafe creation configuration",
    (c) => (entry(c, "shared/destinations/duel", "creation", "configuration").max_players = 2 ** 53),
    "safe range",
  ],
  [
    "a creation configuration over 32 deep",
    (c) => (entry(c, "shared/destinations/duel", "creation", "configuration").max_players = nestedValue(33)),
    "more than 32 deep",
  ],
  [
    "an invalid destination identity",
    (c) =>
      (at(c, "destinations", "entries")["destinations/solo"] = {
        ...entry(c, "shared/destinations/duel"),
        destination: { key: "solo", session_type: "lobby/default", machine_profile: "default" },
      }),
    "invalid or repeated identity",
  ],
  [
    "an app-local destination of another app",
    (c) =>
      (at(c, "destinations", "entries")["apps/arena/destinations/solo"] = {
        ...entry(c, "shared/destinations/duel"),
        destination: { key: "solo", session_type: "lobby/default", machine_profile: "default" },
      }),
    "invalid or repeated identity",
  ],
  [
    "destination identities that differ by case",
    (c) =>
      (at(c, "destinations", "entries")["shared/destinations/DUEL"] = {
        ...entry(c, "shared/destinations/duel"),
        destination: { key: "solo", session_type: "lobby/default", machine_profile: "default" },
      }),
    "invalid or repeated identity",
  ],
  [
    "an empty destination key",
    (c) => (entry(c, "shared/destinations/duel", "destination").key = ""),
    "invalid key, session type or machine profile",
  ],
  [
    "a destination key over 128 bytes",
    (c) => (entry(c, "shared/destinations/duel", "destination").key = "é".repeat(65)),
    "invalid key, session type or machine profile",
  ],
  [
    "a destination key with a control character",
    (c) => (entry(c, "shared/destinations/duel", "destination").key = "du\u0085el"),
    "invalid key, session type or machine profile",
  ],
  [
    "an invalid session type",
    (c) => (entry(c, "shared/destinations/duel", "destination").session_type = "lobby/default/x"),
    "invalid key, session type or machine profile",
  ],
  [
    "an invalid machine profile",
    (c) => (entry(c, "shared/destinations/duel", "destination").machine_profile = "large profile"),
    "invalid key, session type or machine profile",
  ],
  [
    "a repeated destination key",
    (c) => (entry(c, "shared/destinations/duel", "destination").key = "main"),
    "repeats a key",
  ],
  [
    "no empty timeout",
    (c) => (entry(c, "shared/destinations/duel").empty_timeout_seconds = 0),
    "empty timeout outside",
  ],
  [
    "an empty timeout over a day",
    (c) => (entry(c, "shared/destinations/duel").empty_timeout_seconds = 86_401),
    "empty timeout outside",
  ],
  [
    "a creation configuration its schema rejects",
    (c) => (entry(c, "shared/destinations/duel", "creation", "configuration").mode = "trio"),
    "does not accept",
  ],
  [
    "no creation for a configured session",
    (c) => delete entry(c, "shared/destinations/duel").creation,
    "does not accept",
  ],
  [
    "a configuration for an unconfigured session",
    (c) => (entry(c, "apps/arena/destinations/main").creation = { capacity: 1, configuration: { mode: "solo" } }),
    "does not accept",
  ],
  // Domains
  ["an unknown domains field", (c) => (at(c, "domains").extra = 1), "unexpected field extra"],
  ["another domains version", (c) => (at(c, "domains").version = 2), "domains: is not version 1"],
  ["null commands", (c) => (at(c, "domains").commands = null), "not maps"],
  [
    "too many scopes",
    (c) =>
      Object.assign(
        at(c, "domains", "scopes"),
        range(254, (i) => [`s${i}`, { parent: "" }]),
      ),
    "more than 256 scopes",
  ],
  ["an unknown scope field", (c) => (at(c, "domains", "scopes", "lobby").name = "Lobby"), "unexpected field name"],
  ["a non-string scope parent", (c) => (at(c, "domains", "scopes", "lobby").parent = 1), "invalid scope"],
  ["an invalid scope path", (c) => (at(c, "domains", "scopes")["lobby-2"] = { parent: "" }), "invalid scope path"],
  ["scope paths that differ by case", (c) => (at(c, "domains", "scopes").Lobby = { parent: "" }), "invalid scope path"],
  ["no root scope", (c) => (at(c, "domains").scopes = { lobby: { parent: "" } }), "invalid ancestry for scope lobby"],
  ["a root scope with a parent", (c) => (at(c, "domains", "scopes", "").parent = "lobby"), "root scope with a parent"],
  [
    "a scope under the wrong parent",
    (c) => (at(c, "domains", "scopes", "lobby/arena").parent = ""),
    "invalid ancestry",
  ],
  [
    "a scope under a missing parent",
    (c) => (at(c, "domains", "scopes")["ghost/room"] = { parent: "ghost" }),
    "invalid ancestry",
  ],
  ["an invalid app binding", (c) => (at(c, "domains", "apps")["lobby-2"] = "lobby"), "invalid app binding"],
  ["app bindings that differ by case", (c) => (at(c, "domains", "apps").LOBBY = "lobby"), "invalid app binding"],
  ["an app bound to a missing scope", (c) => (at(c, "domains", "apps").lobby = "nowhere"), "invalid app binding"],
  ["an unknown hook field", (c) => (hook(c, "shared/domains/hooks/ping").priority = 1), "unexpected field priority"],
  ["an unknown hook event", (c) => (hook(c, "shared/domains/hooks/ping").event = "server.tick"), "wrong type"],
  ["a hook order outside i32", (c) => (hook(c, "scopes/lobby/hooks/first").order = 2 ** 31), "wrong type"],
  ["a null hook follow_player", (c) => (hook(c, "apps/lobby/app/hooks/enter").follow_player = null), "wrong type"],
  [
    "a hook identity outside its domain",
    (c) => (hook(c, "scopes/lobby/hooks/first").domain = "lobby/arena"),
    "invalid hook identity",
  ],
  [
    "an app hook outside its app's domain",
    (c) => (hook(c, "apps/lobby/app/hooks/enter").domain = ""),
    "invalid hook identity",
  ],
  [
    "hook identities that differ by case",
    (c) =>
      (at(c, "domains", "hooks")["scopes/lobby/hooks/FIRST"] = {
        domain: "lobby",
        event: "player.login",
        export: "loginThird",
        order: 3,
      }),
    "invalid hook identity",
  ],
  [
    "an invalid hook export",
    (c) => (hook(c, "shared/domains/hooks/ping").export = "on-ping"),
    "invalid hook identity, export",
  ],
  [
    "a repeated hook export",
    (c) => (hook(c, "shared/domains/hooks/ping").export = "onEnter"),
    "invalid hook identity, export",
  ],
  [
    "a hook in a missing domain",
    (c) =>
      (at(c, "domains", "hooks")["scopes/ghost/hooks/x"] = { domain: "ghost", event: "player.connect", export: "x" }),
    "invalid hook identity, export or domain",
  ],
  [
    "a responder outside the root domain",
    (c) =>
      (at(c, "domains", "hooks")["scopes/lobby/hooks/route"] = {
        domain: "lobby",
        event: "player.route",
        export: "route",
      }),
    "outside the root domain",
  ],
  [
    "an ordered non-admission hook",
    (c) => (hook(c, "shared/domains/hooks/ping").order = 1),
    "ordered server.ping hook",
  ],
  [
    "a following hook of another event",
    (c) => (hook(c, "scopes/lobby/hooks/first").follow_player = true),
    "following players",
  ],
  [
    "two responders",
    (c) => (at(c, "domains", "hooks")["scopes/hooks/ping"] = { domain: "", event: "server.ping", export: "onPing2" }),
    "several server.ping hooks",
  ],
  [
    "admission hooks with one order",
    (c) => (hook(c, "scopes/lobby/hooks/second").order = 1),
    "without distinct orders",
  ],
  [
    "admission hooks without orders",
    (c) => delete hook(c, "scopes/lobby/hooks/second").order,
    "without distinct orders",
  ],
  ["an unknown command field", (c) => (party(c).description = "Parties"), "unexpected field description"],
  ["a command without follow_player", (c) => delete party(c).follow_player, "is missing follow_player"],
  ["a command field of the wrong type", (c) => (party(c).aliases = "p"), "wrong type"],
  [
    "an unknown route field",
    (c) => (party(c, "routes", 0).permission = "x"),
    "invalid route: has an unexpected field permission",
  ],
  [
    "an unknown argument field",
    (c) => (party(c, "routes", 2, "arguments", 0).step = 1),
    "invalid argument: has an unexpected field step",
  ],
  [
    "an unknown parser",
    (c) => (party(c, "routes", 2, "arguments", 0).parser = "float"),
    "argument with the wrong types",
  ],
  [
    "a bound outside i32",
    (c) => (party(c, "routes", 2, "arguments", 0).max = 2 ** 31),
    "argument with the wrong types",
  ],
  [
    "an invalid suggestion query",
    (c) => (party(c, "routes", 1, "arguments", 0).suggestions = { query: 1 }),
    "argument with the wrong types",
  ],
  ["a command identity outside its domain", (c) => (party(c).domain = "lobby/arena"), "invalid command identity"],
  [
    "command identities that differ by case",
    (c) =>
      (at(c, "domains", "commands")["scopes/lobby/commands/PARTY"] = {
        ...party(c),
        name: "fete",
        aliases: [],
        export: "fete",
      }),
    "invalid command identity",
  ],
  ["a command export shared with a hook", (c) => (party(c).export = "onPing"), "invalid command identity, export"],
  [
    "a command root with two owners",
    (c) => (at(c, "domains", "commands", "apps/arena/app/commands/leave").name = "p"),
    'several owners in scope "lobby/arena"',
  ],
  [
    "a handler export shared with a function",
    (c) => (hook(c, "shared/domains/hooks/ping").export = "get"),
    "also a function export",
  ],
  [
    "too many aliases",
    (c) => (party(c).aliases = Array.from({ length: 17 }, (_, i) => `p${i}`)),
    "invalid or repeated root or alias",
  ],
  ["an invalid alias", (c) => (party(c).aliases = ["P"]), "invalid or repeated root or alias"],
  ["an unknown permission query", (c) => (party(c).permission = "players/nope"), "unknown permission query"],
  ["a permission that is not a query", (c) => (party(c).permission = "matches/start"), "unknown permission query"],
  ["a permission query with arguments", (c) => (party(c).permission = "players/get"), "does not take no arguments"],
  ["no routes", (c) => (party(c).routes = []), "1 to 64 routes"],
  [
    "too many routes",
    (c) => (party(c).routes = Array.from({ length: 65 }, (_, i) => ({ literals: [`r${i}`], arguments: [] }))),
    "1 to 64 routes",
  ],
  [
    "over 256 command nodes",
    (c) =>
      (party(c).routes = Array.from({ length: 11 }, (_, i) => ({
        literals: [`r${i}`, "a", "b", "c", "d", "e", "f", "g"],
        arguments: Array.from({ length: 16 }, (_, j) => ({ name: `a${j}`, parser: "word" })),
      }))),
    "invalid, repeated or excessive route",
  ],
  [
    "too many literals",
    (c) => (party(c, "routes", 1).literals = ["a", "b", "c", "d", "e", "f", "g", "h", "i"]),
    "invalid, repeated or excessive route",
  ],
  ["an invalid literal", (c) => (party(c, "routes", 1).literals = ["Invite"]), "invalid, repeated or excessive route"],
  [
    "a repeated route",
    (c) =>
      list(c, "domains", "commands", "scopes/lobby/commands/party", "routes").push({
        literals: ["invite"],
        arguments: [],
      }),
    "invalid, repeated or excessive route",
  ],
  [
    "too many arguments",
    (c) =>
      (party(c, "routes", 0).arguments = Array.from({ length: 17 }, (_, i) => ({ name: `a${i}`, parser: "word" }))),
    "invalid, repeated or excessive route",
  ],
  [
    "an argument named like a literal child",
    (c) => (party(c, "routes", 0).arguments = [{ name: "invite", parser: "word" }]),
    "argument invite that is also a literal",
  ],
  [
    "an invalid argument name",
    (c) => (party(c, "routes", 3, "arguments", 1).name = "9text"),
    "invalid or repeated argument 9text",
  ],
  [
    "argument names that differ by case",
    (c) => (party(c, "routes", 3, "arguments", 1).name = "Tone"),
    "invalid or repeated argument Tone",
  ],
  [
    "a greedy argument before others",
    (c) => list(c, "domains", "commands", "scopes/lobby/commands/party", "routes", 3, "arguments").reverse(),
    "greedy argument before others",
  ],
  [
    "bounds without an integer parser",
    (c) => (party(c, "routes", 1, "arguments", 0).min = 1),
    "bounds without an integer parser",
  ],
  ["a minimum above its maximum", (c) => (party(c, "routes", 2, "arguments", 0).min = 9), "minimum above its maximum"],
  [
    "suggestions on an integer",
    (c) => (party(c, "routes", 2, "arguments", 0).suggestions = ["2"]),
    "suggestions without a string parser",
  ],
  [
    "too many static suggestions",
    (c) => (party(c, "routes", 3, "arguments", 0).suggestions = Array.from({ length: 65 }, (_, i) => `s${i}`)),
    "invalid static suggestions",
  ],
  [
    "an empty static suggestion",
    (c) => (party(c, "routes", 3, "arguments", 0).suggestions = [""]),
    "invalid static suggestions",
  ],
  [
    "a static suggestion over 256 characters",
    (c) => (party(c, "routes", 3, "arguments", 0).suggestions = ["x".repeat(257)]),
    "invalid static suggestions",
  ],
  [
    "a static suggestion with a control character",
    (c) => (party(c, "routes", 3, "arguments", 0).suggestions = ["lo\nud"]),
    "invalid static suggestions",
  ],
  [
    "a repeated static suggestion",
    (c) => (party(c, "routes", 3, "arguments", 0).suggestions = ["loud", "loud"]),
    "invalid static suggestions",
  ],
  [
    "an unknown suggestion query",
    (c) => (party(c, "routes", 1, "arguments", 0).suggestions = { query: "players/nope" }),
    "unknown suggestion query",
  ],
  [
    "a suggestion query of another shape",
    (c) => (party(c, "routes", 1, "arguments", 0).suggestions = { query: "players/can_manage" }),
    "without input/cursor arguments",
  ],
  ["a non-object table", (c) => (at(c, "tables").players = 42), "invalid table players"],
  ["a function missing its fields", (c) => (at(c, "functions").login = {}), "invalid function login"],
  [
    "an array without items",
    (c) => (at(c, "tables", "matches", "fields", "player").schema = { type: "array" }),
    "invalid array schema",
  ],
  // schema.rs: TableSchema::validate starts table fields at depth 0; Schema::validate allows depth 32.
  [
    "33 nested arrays in a table field",
    (c) => (at(c, "tables", "matches", "fields").nested = field(arrays(33))),
    "more than 32 deep",
  ],
];

const validContracts: [string, (c: Json) => void][] = [
  ["the full realistic contract", () => {}],
  [
    "extra keys on a unit string schema",
    (c) => (at(c, "tables", "players", "fields", "name").schema = { type: "string", max: 3 }),
  ],
  ["32 nested arrays in a table field", (c) => (at(c, "tables", "matches", "fields").nested = field(arrays(32)))],
];

test.each(invalidContracts)("rejects %s", (_name, mutate, problem) => {
  const contract = validContract();
  mutate(contract);
  expect(contractProblem(contract)).toContain(problem);
});

test("rejects a non-object contract", () => {
  expect(contractProblem([])).toBe("is not an object");
});

test.each(validContracts)("accepts %s in both backend files", async (_name, mutate) => {
  const contract = validContract();
  mutate(contract);
  expect(contractProblem(contract)).toBeUndefined();
  const source = "export default {};\n";
  const result = await broken({
    replace: {
      "contract.json": JSON.stringify(contract),
      "backend.json": JSON.stringify({ ...contract, id: "r1", source }),
    },
  });
  expect(JSON.parse(result).id).toBe("r1");
});

test("keeps only the named metadata files, never entries named like Object properties", async () => {
  expect(keepLimit("backend.json")).toBe(5 * 1024 * 1024);
  for (const path of ["constructor", "__proto__", "toString", "hasOwnProperty"])
    expect(keepLimit(path)).toBeUndefined();
  const archive = releaseArchive("r1", undefined, { extra: [["constructor", "x".repeat(1024)]] });
  expect(JSON.parse(await verify(archive)).id).toBe("r1");
});

test("rejects a path that is both a file and a directory, in either order", async () => {
  const scan = (files: [string, string][]) =>
    scanArchive(new Blob([rawArchive(files).bytes]).stream(), limits, () => undefined);
  await expect(
    scan([
      ["assets", "x"],
      ["assets/file.txt", "y"],
    ]),
  ).rejects.toThrow("both a file and a directory");
  await expect(
    scan([
      ["assets/file.txt", "y"],
      ["assets", "x"],
    ]),
  ).rejects.toThrow("both a file and a directory");
  await expect(
    scan([
      ["assets/a/b.txt", "y"],
      ["assets/a", "x"],
    ]),
  ).rejects.toThrow("both a file and a directory");
});

test("rejects archives that differ from their declaration or exceed the budget", async () => {
  const archive = releaseArchive("r1");
  await expect(verify({ ...archive, sha256: "0".repeat(64) })).rejects.toThrow("declared size and digest");
  await expect(verify({ ...archive, sizeBytes: archive.sizeBytes - 1n })).rejects.toThrow("larger than declared");
  await expect(verify(archive, { maxExpandedBytes: 64 * 1024 })).rejects.toThrow("expands past 65536 bytes");
  await expect(verify(archive, { maxEntries: 3 })).rejects.toThrow("more than 3 entries");
});
