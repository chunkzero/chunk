import assert from "node:assert/strict";
import test from "node:test";

import { appConfigurations, appDestinations, isScope } from "../src/apps.ts";
import { defineApp, defineScope, createHook, v } from "../src/index.ts";

test("one implementation accepts distinct immutable destination configurations", () => {
  const definition = defineApp({
    id: "arena",
    runtime: { machineProfile: "small", maxPlayers: 16 },
    implementations: { default: { config: v.object({ label: v.string() }) } },
    destinations: {
      standard: { implementation: "default", key: "public-arena", config: { label: "Standard" } },
      large: { implementation: "default", key: "large-arena", maxPlayers: 32, config: { label: "Large" } },
    },
  });
  assert.deepEqual(appConfigurations(definition), [
    { app: "arena", session: "default", configuration: v.object({ label: v.string() }).schema },
  ]);
  const entries = Object.fromEntries(appDestinations(definition, {}));
  assert.deepEqual(entries["apps/arena/destinations/standard"].creation, {
    capacity: 16,
    configuration: { label: "Standard" },
  });
  assert.equal(entries["apps/arena/destinations/large"].creation.capacity, 32);
  assert.equal(entries["apps/arena/destinations/large"].destination.session_type, "arena/default");
  assert.throws(() => {
    definition.destinations.standard.config.label = "changed";
  });
});

test("default implementations accept empty config and scope behavior stays explicit", () => {
  const defaults = { machineProfile: "local", maxPlayers: 16 };
  const definition = defineApp({ id: "lobby", destinations: { main: { implementation: "default", key: "lobby" } } });
  assert.deepEqual(appDestinations(definition, defaults)[0][1].creation, { capacity: 16, configuration: {} });
  assert.throws(() =>
    appDestinations(
      defineApp({
        id: "lobby",
        destinations: { main: { implementation: "default", key: "lobby", config: { unexpected: true } } },
      }),
      defaults,
    ),
  );
  const declaration = defineScope({ hooks: { gate: createHook("player.login", () => ({ allow: true })) } });
  assert.equal(isScope(declaration), true);
  assert.throws(() => defineScope({ hooks: { broken: () => true } }));
});
