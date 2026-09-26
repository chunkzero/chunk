import { expect, test } from "bun:test";

import { type ArchiveLimits, scanArchive } from "../src/releases/archive.ts";
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

test("rejects table and function declarations chunk_contract cannot read, even when both files agree", async () => {
  const both = (declarations: { tables?: object; functions?: object }) => {
    const contract = {
      contract_version: 2,
      runtime_profile: "transactional_v1",
      tables: {},
      functions: {},
      ...declarations,
    };
    const source = "export default {};\n";
    return {
      replace: {
        "contract.json": JSON.stringify(contract),
        "backend.json": JSON.stringify({ ...contract, id: "r1", source }),
      },
    };
  };
  await expect(broken(both({ tables: { players: 42 }, functions: { login: {} } }))).rejects.toThrow(
    "invalid table players",
  );
  await expect(broken(both({ functions: { login: {} } }))).rejects.toThrow("invalid function login");
  const field = (schema: object) => ({ players: { fields: { name: { schema } } } });
  await expect(broken(both({ tables: field({ type: "strng" }) }))).rejects.toThrow("unknown schema type");
  await expect(broken(both({ tables: field({ type: "array" }) }))).rejects.toThrow("invalid array schema");
  await expect(broken(both({ tables: field({ type: "string", max: 3 }) }))).rejects.toThrow("invalid string schema");

  const valid = both({
    tables: {
      players: {
        fields: {
          name: { schema: { type: "string" } },
          rank: { schema: { type: "nullable", value: { type: "enum", values: ["gold"] } }, optional: true },
          state: {
            schema: {
              type: "union",
              variants: { online: { type: "object", fields: { at: { schema: { type: "number" } } } } },
            },
          },
        },
        indexes: { by_name: ["name"] },
      },
    },
    functions: {
      "players/get": {
        kind: "query",
        visibility: "public",
        export: "get",
        arguments: { type: "object", fields: { id: { schema: { type: "id", table: "players" } } } },
        result: { type: "array", items: { type: "literal", value: 1 } },
      },
    },
  });
  expect(JSON.parse(await broken(valid)).id).toBe("r1");
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
