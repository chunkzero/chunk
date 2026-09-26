import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { Code } from "@connectrpc/connect";

import { DomainService, DomainState } from "../src/gen/chunk/management/v1/domains_pb.ts";
import { ProjectService } from "../src/gen/chunk/management/v1/projects_pb.ts";
import { SecretService } from "../src/gen/chunk/management/v1/secrets_pb.ts";
import { secretContext } from "../src/secrets/service.ts";
import { codeOf, databaseUrl, type Harness, startHarness } from "./harness.ts";

describe.skipIf(!databaseUrl)("SecretService and DomainService", () => {
  let h: Harness;
  let environments: string[];
  beforeAll(async () => {
    h = await startHarness();
    const projects = h.client(ProjectService);
    const projectId =
      (await projects.createProject({ requestId: crypto.randomUUID(), name: "game" })).project?.id ?? "";
    environments = [];
    for (const name of ["production", "staging"]) {
      const created = await projects.createEnvironment({ requestId: crypto.randomUUID(), projectId, name });
      environments.push(created.environment?.id ?? "");
    }
  });
  afterAll(() => h.close());

  test("secrets are write-only, encrypted at rest and versioned", async () => {
    const secrets = h.client(SecretService);
    const environmentId = environments[0] ?? "";
    const value = new TextEncoder().encode("hunter2-database-password");
    const set = (bytes: Uint8Array) =>
      secrets.setSecret({ requestId: crypto.randomUUID(), environmentId, name: "DATABASE_PASSWORD", value: bytes });

    expect((await set(value)).secret?.version).toBe(1n);
    expect((await set(value)).secret?.version).toBe(2n);
    const [stored] = await h.sql<{ ciphertext: Uint8Array }[]>`
      select ciphertext from secrets where environment_id = ${environmentId}`;
    const ciphertext = stored?.ciphertext ?? new Uint8Array();
    expect(Buffer.from(ciphertext).includes(Buffer.from(value))).toBe(false);
    const opened = await h.keys.cipher.open(ciphertext, secretContext(environmentId, "DATABASE_PASSWORD"));
    expect(Buffer.from(opened).equals(Buffer.from(value))).toBe(true);
    await expect(
      h.keys.cipher.open(ciphertext, secretContext(environments[1] ?? "", "DATABASE_PASSWORD")),
    ).rejects.toThrow();

    await secrets.deleteSecret({ environmentId, name: "DATABASE_PASSWORD" });
    expect((await secrets.listSecrets({ environmentId })).secrets).toEqual([]);
    expect((await set(value)).secret?.version).toBe(3n);
    expect(await codeOf(secrets.setSecret({ requestId: crypto.randomUUID(), environmentId, name: "1bad" }))).toBe(
      Code.InvalidArgument,
    );
  });

  test("domains verify through a DNS TXT record", async () => {
    const domains = h.client(DomainService);
    const [first = "", second = ""] = environments;
    const added = (await domains.addDomain({ environmentId: first, hostname: "Play.Example.com." })).domain;
    expect(added?.hostname).toBe("play.example.com");
    expect(added?.state).toBe(DomainState.PENDING_VERIFICATION);
    const [challenge, route] = added?.dnsRecords ?? [];
    expect(challenge?.type).toBe("TXT");
    expect(route).toEqual(
      expect.objectContaining({
        type: "SRV",
        name: "_minecraft._tcp.play.example.com",
        value: `0 0 25565 ${first.replace("_", "-")}.play.example.net`,
      }),
    );
    expect(added?.dnsRecords).toHaveLength(2);
    expect((await domains.addDomain({ environmentId: first, hostname: "play.example.com" })).domain?.id).toBe(
      added?.id ?? "",
    );

    const rival = (await domains.addDomain({ environmentId: second, hostname: "play.example.com" })).domain;
    expect((await domains.verifyDomain({ domainId: added?.id ?? "" })).domain?.state).toBe(
      DomainState.PENDING_VERIFICATION,
    );
    h.txt.set(challenge?.name ?? "", ["unrelated", challenge?.value ?? ""]);
    expect((await domains.verifyDomain({ domainId: added?.id ?? "" })).domain?.state).toBe(DomainState.VERIFIED);

    const rivalChallenge = rival?.dnsRecords[0];
    h.txt.set(rivalChallenge?.name ?? "", [rivalChallenge?.value ?? ""]);
    expect(await codeOf(domains.verifyDomain({ domainId: rival?.id ?? "" }))).toBe(Code.AlreadyExists);

    await domains.removeDomain({ domainId: added?.id ?? "" });
    await domains.removeDomain({ domainId: added?.id ?? "" });
    expect((await domains.listDomains({ environmentId: first })).domains).toEqual([]);
  });
});
