/**
 * Checks `contract.json` the way the Rust side reads it: chunk-build deserializes it as `BackendMetadata`
 * (crates/chunk-build/src/lib.rs) and validates it as a `chunk_contract::Deployment` (crates/chunk-contract/src).
 * Only the JSON object encodings chunk-build writes are accepted.
 */

type Problem = string | undefined;
type Json = Record<string, unknown>;

type UnitType = "null" | "boolean" | "number" | "integer" | "string" | "player" | "session";
type Schema =
  | { type: UnitType }
  | { type: "id"; table: string }
  | { type: "literal"; value: null | boolean | number | string }
  | { type: "enum"; values: string[] }
  | { type: "nullable"; value: Schema }
  | { type: "array"; items: Schema }
  | { type: "object"; fields: Fields }
  | { type: "union"; variants: Record<string, Schema> };
type Fields = Record<string, { schema: Schema; optional?: boolean }>;
interface FunctionDeclaration {
  kind: string;
  export: string;
  arguments: Schema;
  result: Schema;
}
interface Configuration {
  app: string;
  session: string;
  configuration: Schema;
}
interface Hook {
  domain: string;
  event: string;
  export: string;
  order?: number | null;
  follow_player?: boolean;
}
interface Command {
  domain: string;
  name: string;
  aliases: string[];
  export: string;
  permission?: string | null;
  routes: { literals: string[]; arguments: Argument[] }[];
}
interface Argument {
  name: string;
  parser: string;
  min?: number | null;
  max?: number | null;
  suggestions?: string[] | { query: string } | null;
}

const maxDepth = 32;
const maxFields = 64;
const int32 = [-(2 ** 31), 2 ** 31 - 1] as const;
const optionalContracts = ["domains", "session_methods", "session_configurations", "destinations"];
const unitTypes: readonly string[] = ["null", "boolean", "number", "integer", "string", "player", "session"];
const scalarTypes: readonly string[] = ["boolean", "number", "integer", "string", "enum", "id", "player", "session"];
const hookEvents = [
  "server.ping",
  "player.login",
  "player.route",
  "player.beforeMove",
  "player.connect",
  "player.disconnect",
  "domain.enter",
  "domain.leave",
];
const singleResultEvents = ["server.ping", "player.route"];
const admissionEvents = ["player.login", "player.beforeMove"];
const followingEvents = ["player.connect", "domain.enter", "domain.leave"];
const parsers = ["boolean", "integer", "word", "string", "greedy"];
const encoder = new TextEncoder();

/** Describes the first way `contract` differs from what chunk_contract accepts, or undefined when it has none. */
export function contractProblem(contract: unknown): Problem {
  // lib.rs (chunk-build): BackendMetadata, deny_unknown_fields over the flattened deployment.rs: Contracts
  const c = struct(contract, ["contract_version", "runtime_profile", "tables", "functions"], optionalContracts);
  if (typeof c === "string") return c;
  // deployment.rs: Deployment::validate, CONTRACT_VERSION
  if (c.contract_version !== 2) return "is not contract version 2";
  // deployment.rs: RuntimeProfile
  if (c.runtime_profile !== "transactional_v1") return "has an unknown runtime_profile";
  const problem =
    tablesProblem(c.tables) ??
    functionsProblem(c.functions) ??
    contained(c, "session_methods", sessionMethodsProblem) ??
    contained(c, "session_configurations", sessionConfigurationsProblem);
  if (problem !== undefined) return problem;
  const functions = c.functions as Record<string, FunctionDeclaration>;
  const catalog = c.session_configurations as { configurations: Configuration[] } | null | undefined;
  const configurations = catalog?.configurations ?? [];
  return (
    contained(c, "destinations", (value) => destinationsProblem(value, configurations)) ??
    contained(c, "domains", (value) => domainsProblem(value, functions))
  );
}

/** Checks an optional contract, which serde reads as absent when it is missing or null. */
function contained(contract: Json, key: string, check: (value: unknown) => Problem): Problem {
  const value = contract[key];
  return value === undefined || value === null ? undefined : within(`has an invalid ${key}`, check(value));
}

// schema.rs: validate
function tablesProblem(value: unknown): Problem {
  if (!isRecord(value)) return "has no tables map";
  const names = Object.keys(value);
  // schema.rs: validate, MAX_TABLES
  if (names.length > 128) return "has more than 128 tables";
  for (const [name, table] of Object.entries(value)) {
    // schema.rs: validate_name
    const problem = isSchemaName(name) ? tableProblem(table) : "has an invalid name";
    if (problem !== undefined) return within(`has an invalid table ${name}`, problem);
  }
  // schema.rs: validate, table names differ only by case
  if (!distinctFolded(names)) return "has table names that differ only by case";
  // schema.rs: validate, MAX_SCHEMA_BYTES of the tables as serde writes them
  const tables = value as Record<string, { fields: Fields; indexes?: Record<string, string[]> }>;
  const written = mapValues(tables, (table) => ({ fields: writtenFields(table.fields), indexes: table.indexes ?? {} }));
  return jsonBytes(written) > 1024 * 1024 ? "has tables over 1 MiB" : undefined;
}

function tableProblem(value: unknown): Problem {
  // schema.rs: TableSchema
  const table = struct(value, ["fields"], ["indexes"]);
  if (typeof table === "string") return table;
  // schema.rs: TableSchema::validate
  const problem = fieldsProblem(table.fields, 0, false);
  if (problem !== undefined || !Object.hasOwn(table, "indexes")) return problem;
  const fields = table.fields as Fields;
  const { indexes } = table;
  if (!isRecord(indexes)) return "has no indexes map";
  // schema.rs: TableSchema::validate, MAX_INDEXES
  if (Object.keys(indexes).length > 16) return "has more than 16 indexes";
  for (const [name, columns] of Object.entries(indexes)) {
    // schema.rs: TableSchema::validate, index names and MAX_INDEX_FIELDS
    if (!isSchemaName(name) || !isStrings(columns) || columns.length === 0 || columns.length > 8) {
      return `has an invalid index ${name}`;
    }
    // schema.rs: TableSchema::validate, distinct declared scalar fields
    const scalar = (column: string) => scalarTypes.includes(own(fields, column)?.schema.type ?? "");
    if (new Set(columns).size !== columns.length || !columns.every(scalar)) {
      return `has an index ${name} without distinct declared scalar fields`;
    }
  }
  // schema.rs: TableSchema::validate, index names differ only by case
  return distinctFolded(Object.keys(indexes)) ? undefined : "has index names that differ only by case";
}

// schema.rs: validate_fields
function fieldsProblem(value: unknown, depth: number, metadata: boolean): Problem {
  if (!isRecord(value)) return "has no fields map";
  const names = Object.keys(value);
  // schema.rs: validate_fields, MAX_FIELDS plus an object's storage _id
  if (names.length > maxFields + (metadata && Object.hasOwn(value, "_id") ? 1 : 0)) return "has more than 64 fields";
  for (const [name, entry] of Object.entries(value)) {
    // schema.rs: Field
    const field = struct(entry, ["schema"], ["optional"]);
    if (typeof field === "string") return within(`has an invalid field ${name}`, field);
    if (Object.hasOwn(field, "optional") && typeof field.optional !== "boolean") {
      return `has an invalid field ${name}: has a non-boolean optional`;
    }
    const problem = schemaProblem(field.schema, depth);
    if (problem !== undefined) return within(`has an invalid field ${name}`, problem);
    // schema.rs: validate_fields, only objects declare a required storage _id
    const storageId = metadata && name === "_id" && (field.schema as Schema).type === "id" && field.optional !== true;
    if (!storageId && !isSchemaName(name)) return `has an invalid field name ${name}`;
  }
  // schema.rs: validate_fields, field names differ only by case
  return distinctFolded(names) ? undefined : "has field names that differ only by case";
}

// schema.rs: Schema, tagged by `type`, and Schema::validate
function schemaProblem(value: unknown, depth: number): Problem {
  // schema.rs: Schema::validate, MAX_DEPTH
  if (depth > maxDepth) return "nests schemas more than 32 deep";
  if (!isRecord(value) || typeof value.type !== "string") return "has a schema without a type";
  const { type } = value;
  // schema.rs: Schema's unit variants, whose other keys serde ignores
  if (unitTypes.includes(type)) return undefined;
  const variant = (key: string) =>
    typeof struct(value, ["type", key]) === "string" ? `has an invalid ${type} schema` : undefined;
  switch (type) {
    case "id":
      // schema.rs: Schema::validate, id tables
      return variant("table") ?? (isSchemaName(value.table) ? undefined : "has an id schema with an invalid table");
    case "literal":
      // schema.rs: Schema::validate, scalar literals within the wire limits
      return (
        variant("value") ??
        (isRecord(value.value) || Array.isArray(value.value) ? "has a non-scalar literal" : wireProblem(value.value, 0))
      );
    case "enum": {
      // schema.rs: Schema::validate, enum values
      const { values } = value;
      const valid =
        isStrings(values) &&
        values.length > 0 &&
        values.length <= maxFields &&
        new Set(values).size === values.length &&
        values.every(isSchemaName);
      return variant("values") ?? (valid ? undefined : "has an enum schema with invalid values");
    }
    case "nullable":
      return variant("value") ?? schemaProblem(value.value, depth + 1);
    case "array":
      return variant("items") ?? schemaProblem(value.items, depth + 1);
    case "object":
      return variant("fields") ?? fieldsProblem(value.fields, depth + 1, true);
    case "union":
      return variant("variants") ?? variantsProblem(value.variants, depth);
    default:
      return `has an unknown schema type ${JSON.stringify(type)}`;
  }
}

// schema.rs: Schema::validate, unions
function variantsProblem(value: unknown, depth: number): Problem {
  if (!isRecord(value)) return "has a union schema without variants";
  const entries = Object.entries(value);
  // schema.rs: MAX_UNION_VARIANTS
  if (entries.length === 0 || entries.length > 16) return "has a union schema without 1 to 16 variants";
  for (const [name, variant] of entries) {
    const problem = schemaProblem(variant, depth + 1);
    if (problem !== undefined) return within(`has an invalid union variant ${name}`, problem);
    // schema.rs: Schema::validate, variant names
    if (!isSchemaName(name)) return `has an invalid union variant name ${name}`;
    // schema.rs: Schema::validate, object variants without a type field
    const schema = variant as Schema;
    if (schema.type !== "object" || Object.hasOwn(schema.fields, "type")) {
      return `has a union variant ${name} that is not an object without a type field`;
    }
  }
  return undefined;
}

// deployment.rs: validate_wire_value
function wireProblem(value: unknown, depth: number): Problem {
  if (depth > maxDepth) return "nests a value more than 32 deep";
  if (typeof value === "number") {
    const safe = Number.isFinite(value) && !(Number.isInteger(value) && Math.abs(value) > Number.MAX_SAFE_INTEGER);
    return safe ? undefined : "has a number outside JavaScript's safe range";
  }
  if (Array.isArray(value)) return first(value, (item) => wireProblem(item, depth + 1));
  if (isRecord(value)) return first(Object.values(value), (item) => wireProblem(item, depth + 1));
  return undefined;
}

// deployment.rs: Deployment::validate, functions
function functionsProblem(value: unknown): Problem {
  if (!isRecord(value)) return "has no functions map";
  const paths = Object.keys(value);
  if (paths.length > 256) return "has more than 256 functions";
  const exports = new Set<string>();
  for (const [path, entry] of Object.entries(value)) {
    // deployment.rs: Function, FunctionKind and Visibility
    const declaration = struct(entry, ["kind", "visibility", "export", "arguments", "result"]);
    if (typeof declaration === "string") return within(`has an invalid function ${path}`, declaration);
    if (!oneOf(declaration.kind, ["query", "mutation", "action"])) return `has an unknown kind for ${path}`;
    if (!oneOf(declaration.visibility, ["public", "internal"])) return `has an unknown visibility for ${path}`;
    // deployment.rs: Deployment::validate, function paths
    if (path.length > 256 || !path.split("/").every((segment) => isIdentifier(segment))) {
      return `has an invalid function path ${path}`;
    }
    // deployment.rs: Deployment::validate, exports are distinct identifiers
    if (!isIdentifier(declaration.export) || !insert(exports, declaration.export)) {
      return `has an invalid or repeated export for ${path}`;
    }
    // deployment.rs: Deployment::validate, argument and result schemas
    const problem = schemaProblem(declaration.arguments, 0) ?? schemaProblem(declaration.result, 0);
    if (problem !== undefined) return within(`has an invalid function ${path}`, problem);
  }
  // deployment.rs: Deployment::validate, function namespace collision
  const folded = new Set(paths.map(fold));
  if (folded.size !== paths.length) return "has function paths that differ only by case";
  // deployment.rs: Deployment::validate, function path collides with namespace
  for (const path of folded) {
    const segments = path.split("/");
    if (segments.some((_, end) => end > 0 && folded.has(segments.slice(0, end).join("/")))) {
      return `has a function path ${path} inside another function's namespace`;
    }
  }
  return undefined;
}

// session_methods.rs: RawSessionMethods and SessionMethods::validate
function sessionMethodsProblem(value: unknown): Problem {
  const contract = struct(value, ["version", "methods"]);
  if (typeof contract === "string") return contract;
  if (contract.version !== 1) return "is not version 1";
  const { methods } = contract;
  if (!Array.isArray(methods)) return "has no methods list";
  if (methods.length > 256) return "has more than 256 methods";
  const identities = new Set<string>();
  for (const entry of methods) {
    // session_methods.rs: SessionMethodDeclaration
    const method = struct(entry, ["app", "session", "name", "arguments", "result"]);
    if (typeof method === "string") return within("has an invalid method", method);
    // session_methods.rs: SessionMethods::validate, identities
    const { app, session, name } = method;
    if (!isIdentifier(app) || !isIdentifier(session) || !isIdentifier(name)) return "has an invalid method identity";
    const identity = `${app}/${session}/${name}`;
    if (!insert(identities, fold(identity))) return `declares method ${identity} twice`;
    // session_methods.rs: SessionMethods::validate, object arguments and valid schemas
    const problem = schemaProblem(method.arguments, 0) ?? schemaProblem(method.result, 0);
    if (problem !== undefined) return within(`has an invalid method ${identity}`, problem);
    if ((method.arguments as Schema).type !== "object") return `has non-object arguments for method ${identity}`;
  }
  return undefined;
}

// session_configurations.rs: RawSessionConfigurations and SessionConfigurations::validate
function sessionConfigurationsProblem(value: unknown): Problem {
  const catalog = struct(value, ["version", "configurations"]);
  if (typeof catalog === "string") return catalog;
  if (catalog.version !== 1) return "is not version 1";
  const { configurations } = catalog;
  if (!Array.isArray(configurations)) return "has no configurations list";
  if (configurations.length > 256) return "has more than 256 configurations";
  const identities = new Set<string>();
  for (const entry of configurations) {
    // session_configurations.rs: SessionConfigurationDeclaration
    const declaration = struct(entry, ["app", "session", "configuration"]);
    if (typeof declaration === "string") return within("has an invalid configuration", declaration);
    // session_configurations.rs: SessionConfigurations::validate, identities
    const { app, session } = declaration;
    if (!isIdentifier(app) || !isIdentifier(session)) return "has an invalid configuration identity";
    if (!insert(identities, fold(`${app}/${session}`))) return `declares configuration ${app}/${session} twice`;
    // session_configurations.rs: SessionConfigurations::validate, object schemas
    const problem = schemaProblem(declaration.configuration, 0);
    if (problem !== undefined) return within(`has an invalid configuration ${app}/${session}`, problem);
    if ((declaration.configuration as Schema).type !== "object") return `has a non-object ${app}/${session}`;
  }
  // session_configurations.rs: SessionConfigurations::validate, 2 MiB as serde writes the catalog
  const written = (configurations as Configuration[]).map(({ app, session, configuration }) => ({
    app,
    session,
    configuration: writtenSchema(configuration),
  }));
  return jsonBytes({ version: 1, configurations: written }) > 2 * 1024 * 1024 ? "is over 2 MiB" : undefined;
}

// destinations.rs: DestinationManifest and DestinationManifest::validate
function destinationsProblem(value: unknown, configurations: readonly Configuration[]): Problem {
  const manifest = struct(value, ["version", "entries"]);
  if (typeof manifest === "string") return manifest;
  const { entries } = manifest;
  if (!isRecord(entries)) return "has no entries map";
  const count = Object.keys(entries).length;
  if (manifest.version !== 1 || count === 0 || count > 256) return "is not version 1 with 1 to 256 entries";
  const ids = new Set<string>();
  const keys = new Set<string>();
  for (const [id, entry] of Object.entries(entries)) {
    const problem = destinationProblem(id, entry, ids, keys, configurations);
    if (problem !== undefined) return within(`has an invalid entry ${id}`, problem);
  }
  return undefined;
}

function destinationProblem(
  id: string,
  value: unknown,
  ids: Set<string>,
  keys: Set<string>,
  configurations: readonly Configuration[],
): Problem {
  // destinations.rs: DestinationPolicy, Destination and DestinationOverflow
  const policy = struct(value, ["destination", "overflow", "empty_timeout_seconds"], ["creation"]);
  if (typeof policy === "string") return policy;
  const destination = struct(policy.destination, ["key", "session_type", "machine_profile"]);
  if (typeof destination === "string") return within("has an invalid destination", destination);
  const { key, session_type: sessionType, machine_profile: profile } = destination;
  if (typeof key !== "string" || typeof sessionType !== "string" || typeof profile !== "string") {
    return "has a destination with non-string fields";
  }
  if (!oneOf(policy.overflow, ["replicate", "reject"])) return "has an unknown overflow";
  let configuration: unknown = {};
  if (policy.creation !== undefined && policy.creation !== null) {
    // destinations.rs: SessionCreation
    const creation = struct(policy.creation, ["capacity", "configuration"]);
    if (typeof creation === "string") return within("has an invalid creation", creation);
    // destinations.rs: DestinationManifest::validate, creation capacity
    if (!isInteger(creation.capacity, 1, 128)) return "has a creation capacity outside 1 to 128";
    // session_configurations.rs: validate_configuration_value
    const problem = configurationValueProblem(creation.configuration);
    if (problem !== undefined) return within("has an invalid creation configuration", problem);
    configuration = creation.configuration;
  }
  // destinations.rs: DestinationManifest::validate, shared or app-local identities
  const local = id.startsWith("apps/") ? splitOnce(id.slice(5), "/destinations/") : undefined;
  const isLocal =
    local !== undefined &&
    isIdentifier(local[0]) &&
    isIdentifier(local[1]) &&
    splitOnce(sessionType, "/")?.[0] === local[0];
  const isShared = id.startsWith("shared/destinations/") && isIdentifier(id.slice(20));
  if (!(isLocal || isShared) || !insert(ids, fold(id))) return "has an invalid or repeated identity";
  // destinations.rs: Destination::validate
  const session = splitOnce(sessionType, "/");
  if (
    key === "" ||
    byteLength(key) > 128 ||
    /\p{Cc}/u.test(key) ||
    session === undefined ||
    !isIdentifier(session[0]) ||
    !isIdentifier(session[1]) ||
    !/^[A-Za-z0-9_-]{1,128}$/.test(profile)
  ) {
    return "has an invalid key, session type or machine profile";
  }
  // destinations.rs: DestinationManifest::validate, keys per session type
  if (!insert(keys, JSON.stringify([sessionType, key]))) return "repeats a key for its session type";
  // destinations.rs: DestinationManifest::validate, empty_timeout_seconds
  if (!isInteger(policy.empty_timeout_seconds, 1, 86_400)) return "has an empty timeout outside 1 to 86400 seconds";
  // destinations.rs: DestinationManifest::validate_configurations
  return sessionConfigurationProblem(configurations, session, configuration);
}

// session_configurations.rs: validate_session_configuration
function sessionConfigurationProblem(
  configurations: readonly Configuration[],
  [app, session]: [string, string],
  value: unknown,
): Problem {
  const problem = configurationValueProblem(value);
  if (problem !== undefined) return problem;
  const declaration = configurations.find((entry) => entry.app === app && entry.session === session);
  const accepted =
    declaration === undefined
      ? isRecord(value) && Object.keys(value).length === 0
      : accepts(declaration.configuration, value, 0);
  return accepted ? undefined : "has a creation configuration its session's schema does not accept";
}

// session_configurations.rs: validate_configuration_value
function configurationValueProblem(value: unknown): Problem {
  if (!isRecord(value) || jsonBytes(value) > 65_536) return "is not an object within 65536 bytes";
  return wireProblem(value, 0);
}

// domains.rs: DomainManifest and DomainManifest::validate
function domainsProblem(value: unknown, functions: Record<string, FunctionDeclaration>): Problem {
  const manifest = struct(value, ["version", "scopes", "apps", "hooks"], ["commands"]);
  if (typeof manifest === "string") return manifest;
  if (manifest.version !== 1) return "is not version 1";
  const { scopes, apps, hooks } = manifest;
  const commands = Object.hasOwn(manifest, "commands") ? manifest.commands : {};
  if (!isRecord(scopes) || !isRecord(apps) || !isRecord(hooks) || !isRecord(commands)) {
    return "has scopes, apps, hooks or commands that are not maps";
  }
  // domains.rs: DomainManifest::validate, size limits
  if ([scopes, apps, hooks, commands].some((map) => Object.keys(map).length > 256)) {
    return "has more than 256 scopes, apps, hooks or commands";
  }
  const problem =
    scopesProblem(scopes) ??
    appsProblem(apps, scopes) ??
    first(Object.entries(hooks), ([id, hook]) => within(`has an invalid hook ${id}`, hookShapeProblem(hook))) ??
    first(Object.entries(commands), ([id, command]) =>
      within(`has an invalid command ${id}`, commandShapeProblem(command)),
    );
  if (problem !== undefined) return problem;
  const identities = new Set<string>();
  const exports = new Set<string>();
  const bindings = apps as Record<string, string>;
  const commandsById = commands as Record<string, Command>;
  return (
    hooksProblem(hooks as Record<string, Hook>, bindings, scopes, identities, exports) ??
    commandsProblem(commandsById, bindings, scopes, identities, exports) ??
    first(Object.entries(commandsById), ([id, command]) =>
      within(`has an invalid command ${id}`, commandProblem(command, functions)),
    ) ??
    // deployment.rs: Deployment::validate, domain handler export collides with function export
    (Object.values(functions).some((declaration) => exports.has(declaration.export))
      ? "has a handler export that is also a function export"
      : undefined)
  );
}

function scopesProblem(scopes: Json): Problem {
  const paths = new Set<string>();
  for (const [path, entry] of Object.entries(scopes)) {
    // domains.rs: DomainScope
    const scope = struct(entry, [], ["parent"]);
    if (typeof scope === "string") return within(`has an invalid scope ${JSON.stringify(path)}`, scope);
    const parent = scope.parent ?? null;
    if (parent !== null && typeof parent !== "string") return `has an invalid scope ${JSON.stringify(path)}`;
    // domains.rs: domain_path, case-colliding domain paths
    if (!isDomainPath(path) || !insert(paths, fold(path))) return `has an invalid scope path ${JSON.stringify(path)}`;
    // domains.rs: DomainManifest::validate, root scope
    if (path === "" && parent !== null) return "has a root scope with a parent";
    // domains.rs: DomainManifest::validate, domain ancestry
    const expected = path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "";
    if (path !== "" && (parent !== expected || !Object.hasOwn(scopes, expected))) {
      return `has invalid ancestry for scope ${path}`;
    }
  }
  // domains.rs: DomainManifest::validate, root scope
  return Object.hasOwn(scopes, "") ? undefined : "has no root scope";
}

// domains.rs: DomainManifest::validate, app domain bindings
function appsProblem(apps: Json, scopes: Json): Problem {
  const names = new Set<string>();
  for (const [app, domain] of Object.entries(apps)) {
    if (typeof domain !== "string" || !isIdentifier(app) || !insert(names, fold(app)) || !Object.hasOwn(scopes, domain)) {
      return `has an invalid app binding ${app}`;
    }
  }
  return undefined;
}

// domains.rs: Hook and HookEvent
function hookShapeProblem(value: unknown): Problem {
  const hook = struct(value, ["domain", "event", "export"], ["order", "follow_player"]);
  if (typeof hook === "string") return hook;
  const order = hook.order ?? null;
  const valid =
    typeof hook.domain === "string" &&
    oneOf(hook.event, hookEvents) &&
    typeof hook.export === "string" &&
    (order === null || isInteger(order, ...int32)) &&
    (!Object.hasOwn(hook, "follow_player") || typeof hook.follow_player === "boolean");
  return valid ? undefined : "has fields of the wrong type";
}

// domains.rs: DomainManifest::validate_handlers, hooks
function hooksProblem(
  hooks: Record<string, Hook>,
  apps: Record<string, string>,
  scopes: Json,
  identities: Set<string>,
  exports: Set<string>,
): Problem {
  const groups = new Map<string, { event: string; orders: (number | null)[] }>();
  for (const [identity, hook] of Object.entries(hooks)) {
    const { domain, event } = hook;
    // domains.rs: DomainManifest::validate_handlers, hook identity, export or domain
    if (
      !handlerIdentity(apps, identity, domain, "hooks") ||
      !insert(identities, fold(identity)) ||
      !isIdentifier(hook.export) ||
      !insert(exports, hook.export) ||
      !Object.hasOwn(scopes, domain)
    ) {
      return `has an invalid hook identity, export or domain for ${identity}`;
    }
    // domains.rs: HookEvent::single_result responders require the root domain
    if (singleResultEvents.includes(event) && domain !== "") return `has a ${event} hook outside the root domain`;
    // domains.rs: HookEvent::admission, only admission hooks accept ordering
    const order = hook.order ?? null;
    if (order !== null && !admissionEvents.includes(event)) return `has an ordered ${event} hook`;
    // domains.rs: HookEvent::can_follow_player
    if (hook.follow_player === true && !followingEvents.includes(event)) return `has a ${event} hook following players`;
    const group = JSON.stringify([domain, event]);
    groups.set(group, { event, orders: [...(groups.get(group)?.orders ?? []), order] });
  }
  for (const { event, orders } of groups.values()) {
    // domains.rs: DomainManifest::validate_handlers, ambiguous single-result hook responders
    if (singleResultEvents.includes(event) && orders.length > 1) return `has several ${event} hooks in one domain`;
    // domains.rs: DomainManifest::validate_handlers, same-scope admission hooks need distinct explicit orders
    const distinct = new Set(orders.filter((order) => order !== null)).size === orders.length;
    if (admissionEvents.includes(event) && orders.length > 1 && !distinct) {
      return `has ${event} hooks in one domain without distinct orders`;
    }
  }
  return undefined;
}

// commands.rs: Command, CommandRoute, CommandArgument, CommandParser, CommandSuggestions and SuggestionQuery
function commandShapeProblem(value: unknown): Problem {
  const command = struct(value, ["domain", "name", "aliases", "export", "follow_player", "routes"], ["permission"]);
  if (typeof command === "string") return command;
  const permission = command.permission ?? null;
  const valid =
    typeof command.domain === "string" &&
    typeof command.name === "string" &&
    isStrings(command.aliases) &&
    typeof command.export === "string" &&
    typeof command.follow_player === "boolean" &&
    (permission === null || typeof permission === "string") &&
    Array.isArray(command.routes);
  if (!valid) return "has fields of the wrong type";
  return first(command.routes as unknown[], (entry) => {
    const route = struct(entry, ["literals", "arguments"]);
    if (typeof route === "string") return within("has an invalid route", route);
    if (!isStrings(route.literals) || !Array.isArray(route.arguments)) return "has a route with the wrong types";
    return first(route.arguments as unknown[], argumentShapeProblem);
  });
}

function argumentShapeProblem(value: unknown): Problem {
  const argument = struct(value, ["name", "parser"], ["min", "max", "suggestions"]);
  if (typeof argument === "string") return within("has an invalid argument", argument);
  const bound = (key: "min" | "max") => (argument[key] ?? null) === null || isInteger(argument[key], ...int32);
  const suggestions = argument.suggestions ?? null;
  const query = struct(suggestions, ["query"]);
  const valid =
    typeof argument.name === "string" &&
    oneOf(argument.parser, parsers) &&
    bound("min") &&
    bound("max") &&
    (suggestions === null || isStrings(suggestions) || (typeof query !== "string" && typeof query.query === "string"));
  return valid ? undefined : "has an argument with the wrong types";
}

function commandsProblem(
  commands: Record<string, Command>,
  apps: Record<string, string>,
  scopes: Json,
  identities: Set<string>,
  exports: Set<string>,
): Problem {
  for (const [identity, command] of Object.entries(commands)) {
    // domains.rs: DomainManifest::validate_handlers, command identity, export or domain
    if (
      !handlerIdentity(apps, identity, command.domain, "commands") ||
      !insert(identities, fold(identity)) ||
      !isIdentifier(command.export) ||
      !insert(exports, command.export) ||
      !Object.hasOwn(scopes, command.domain)
    ) {
      return `has an invalid command identity, export or domain for ${identity}`;
    }
  }
  // commands.rs: visible_commands for every scope, with no JVM roots
  for (const domain of Object.keys(scopes)) {
    const occupied = new Set<string>();
    for (const command of Object.values(commands)) {
      const visible =
        command.domain === "" || command.domain === domain || domain.startsWith(`${command.domain}/`);
      if (visible && ![command.name, ...command.aliases].every((root) => insert(occupied, fold(root)))) {
        return `has command roots with several owners in scope ${JSON.stringify(domain)}`;
      }
    }
  }
  return undefined;
}

// commands.rs: Command::validate
function commandProblem(command: Command, functions: Record<string, FunctionDeclaration>): Problem {
  const roots = [command.name, ...command.aliases];
  // commands.rs: Command::validate, roots and aliases
  if (command.aliases.length > 16 || !roots.every(isLiteral) || new Set(roots).size !== roots.length) {
    return "has an invalid or repeated root or alias";
  }
  // commands.rs: Command::validate, permission queries
  const permission = command.permission ?? null;
  if (permission !== null) {
    const query = queryFunction(functions, permission);
    if (query === undefined) return "has an unknown permission query";
    const { arguments: parameters, result } = query;
    if (parameters.type !== "object" || Object.keys(parameters.fields).length > 0 || result.type !== "boolean") {
      return "has a permission query that does not take no arguments and return a boolean";
    }
  }
  // commands.rs: Command::validate, route count limit
  if (command.routes.length === 0 || command.routes.length > 64) return "does not have 1 to 64 routes";
  let nodes = 1 + command.aliases.length;
  const paths = new Set<string>();
  for (const route of command.routes) {
    // commands.rs: Command::validate, invalid, duplicate or excessive command route
    nodes += route.literals.length + route.arguments.length;
    if (
      nodes > 256 ||
      route.literals.length > 8 ||
      !route.literals.every(isLiteral) ||
      !insert(paths, JSON.stringify(route.literals)) ||
      route.arguments.length > 16
    ) {
      return "has an invalid, repeated or excessive route";
    }
    // commands.rs: Command::validate, argument name conflicts with literal child
    const next = route.arguments[0]?.name;
    const depth = route.literals.length;
    if (
      next !== undefined &&
      command.routes.some(
        (other) => other.literals[depth] === next && route.literals.every((literal, i) => other.literals[i] === literal),
      )
    ) {
      return `has an argument ${next} that is also a literal`;
    }
    const names = new Set<string>();
    for (const [index, argument] of route.arguments.entries()) {
      // commands.rs: Command::validate, argument names
      if (!isIdentifier(argument.name, 64) || !insert(names, fold(argument.name))) {
        return `has an invalid or repeated argument ${argument.name}`;
      }
      // commands.rs: Command::validate, greedy command argument must be last
      if (argument.parser === "greedy" && index + 1 !== route.arguments.length) return "has a greedy argument before others";
      const problem = argumentProblem(argument, functions);
      if (problem !== undefined) return within(`has an invalid argument ${argument.name}`, problem);
    }
  }
  return undefined;
}

// commands.rs: CommandArgument::validate
function argumentProblem(argument: Argument, functions: Record<string, FunctionDeclaration>): Problem {
  const min = argument.min ?? null;
  const max = argument.max ?? null;
  const suggestions = argument.suggestions ?? null;
  // commands.rs: CommandArgument::validate, numeric bounds
  if (argument.parser !== "integer" && (min !== null || max !== null)) return "has bounds without an integer parser";
  if (min !== null && max !== null && min > max) return "has a minimum above its maximum";
  // commands.rs: CommandArgument::validate, custom suggestions require a string parser
  if (suggestions !== null && (argument.parser === "integer" || argument.parser === "boolean")) {
    return "has suggestions without a string parser";
  }
  if (Array.isArray(suggestions)) {
    // commands.rs: CommandArgument::validate and valid_suggestion, static suggestions
    if (suggestions.length > 64 || !suggestions.every(isSuggestion) || new Set(suggestions).size !== suggestions.length) {
      return "has invalid static suggestions";
    }
  } else if (suggestions !== null) {
    // commands.rs: CommandArgument::validate, suggestion queries
    const query = queryFunction(functions, suggestions.query);
    if (query === undefined) return "has an unknown suggestion query";
    const { arguments: parameters, result } = query;
    const required = (name: string, type: string) => {
      const field = parameters.type === "object" ? own(parameters.fields, name) : undefined;
      return field !== undefined && field.optional !== true && field.schema.type === type;
    };
    const signature =
      parameters.type === "object" &&
      Object.keys(parameters.fields).length === 2 &&
      required("input", "string") &&
      required("cursor", "integer") &&
      result.type === "array" &&
      result.items.type === "string";
    if (!signature) return "has a suggestion query without input/cursor arguments and a string array result";
  }
  return undefined;
}

// domains.rs: DomainManifest::handler_identity
function handlerIdentity(apps: Record<string, string>, identity: string, domain: string, kind: string): boolean {
  const suffix = domain === "" ? `${kind}/` : `${domain}/${kind}/`;
  for (const prefix of ["shared/domains/", "scopes/"]) {
    const start = `${prefix}${suffix}`;
    if (identity.startsWith(start) && isIdentifier(identity.slice(start.length))) return true;
  }
  const owned = identity.startsWith("apps/") ? splitOnce(identity.slice(5), "/") : undefined;
  if (owned === undefined) return false;
  const [app, path] = owned;
  const start = `app/${kind}/`;
  return own(apps, app) === domain && path.startsWith(start) && isIdentifier(path.slice(start.length));
}

// commands.rs: query
function queryFunction(functions: Record<string, FunctionDeclaration>, path: string): FunctionDeclaration | undefined {
  const declaration = own(functions, path);
  return declaration?.kind === "query" ? declaration : undefined;
}

// schema.rs: Schema::accepts_at
function accepts(schema: Schema, value: unknown, depth: number): boolean {
  if (depth > maxDepth) return false;
  switch (schema.type) {
    case "null":
      return value === null;
    case "boolean":
      return typeof value === "boolean";
    case "number":
      return typeof value === "number";
    case "integer":
      return Number.isInteger(value);
    case "string":
      return typeof value === "string";
    case "id":
      return (
        typeof value === "string" &&
        value.startsWith(`${schema.table}:`) &&
        isDocumentId(value.slice(schema.table.length + 1))
      );
    case "player":
    case "session":
      return typeof value === "string" && isDocumentId(value);
    case "literal":
      return value === schema.value;
    case "enum":
      return typeof value === "string" && schema.values.includes(value);
    case "nullable":
      return value === null || accepts(schema.value, value, depth + 1);
    case "array":
      return Array.isArray(value) && value.every((item) => accepts(schema.items, item, depth + 1));
    case "object":
      return acceptsObject(schema.fields, value, depth);
    case "union": {
      const variant = isRecord(value) && typeof value.type === "string" ? own(schema.variants, value.type) : undefined;
      return variant?.type === "object" && depth < maxDepth && acceptsObject(variant.fields, value, depth + 1, "type");
    }
  }
}

// schema.rs: accepts_object
function acceptsObject(fields: Fields, value: unknown, depth: number, ignored?: string): boolean {
  return (
    isRecord(value) &&
    Object.keys(value).every((key) => key === ignored || Object.hasOwn(fields, key)) &&
    Object.entries(fields).every(([key, field]) =>
      Object.hasOwn(value, key) ? accepts(field.schema, value[key], depth + 1) : field.optional === true,
    )
  );
}

/** A schema as serde writes it back: unit variants lose their ignored keys and fields spell out `optional`. */
function writtenSchema(schema: Schema): Schema {
  switch (schema.type) {
    case "id":
      return { type: schema.type, table: schema.table };
    case "literal":
      return { type: schema.type, value: schema.value };
    case "enum":
      return { type: schema.type, values: schema.values };
    case "nullable":
      return { type: schema.type, value: writtenSchema(schema.value) };
    case "array":
      return { type: schema.type, items: writtenSchema(schema.items) };
    case "object":
      return { type: schema.type, fields: writtenFields(schema.fields) };
    case "union":
      return { type: schema.type, variants: mapValues(schema.variants, writtenSchema) };
    default:
      return { type: schema.type };
  }
}

function writtenFields(fields: Fields): Fields {
  return mapValues(fields, (field) => ({ schema: writtenSchema(field.schema), optional: field.optional ?? false }));
}

/** `value` as a serde struct with deny_unknown_fields, or its first missing or unexpected key. */
function struct(value: unknown, required: readonly string[], optional: readonly string[] = []): Json | string {
  if (!isRecord(value)) return "is not an object";
  const missing = required.find((key) => !Object.hasOwn(value, key));
  if (missing !== undefined) return `is missing ${missing}`;
  const extra = Object.keys(value).find((key) => !required.includes(key) && !optional.includes(key));
  return extra === undefined ? value : `has an unexpected field ${extra}`;
}

// deployment.rs: identifier and ascii_identifier
function isIdentifier(value: unknown, limit = 128): value is string {
  return typeof value === "string" && value.length <= limit && /^[A-Za-z_][A-Za-z0-9_]*$/.test(value);
}

// schema.rs: validate_name
function isSchemaName(value: unknown): value is string {
  return typeof value === "string" && /^[A-Za-z][A-Za-z0-9_]{0,63}$/.test(value) && !fold(value).startsWith("sqlite_");
}

// schema.rs: valid_id
function isDocumentId(value: string): boolean {
  return /^[A-Za-z0-9_-]{1,128}$/.test(value);
}

// domains.rs: domain_path
function isDomainPath(value: string): boolean {
  const segments = value.split("/");
  return value === "" || (value.length <= 256 && segments.length <= 32 && segments.every((s) => isIdentifier(s)));
}

// commands.rs: literal
function isLiteral(value: string): boolean {
  return /^[a-z][a-z0-9_-]{0,63}$/.test(value);
}

// commands.rs: valid_suggestion
function isSuggestion(value: string): boolean {
  return value !== "" && byteLength(value) <= 1024 && [...value].length <= 256 && !/\p{Cc}/u.test(value);
}

/** Rust's `to_ascii_lowercase`. */
function fold(value: string): string {
  return value.replace(/[A-Z]/g, (letter) => letter.toLowerCase());
}

function distinctFolded(values: string[]): boolean {
  return new Set(values.map(fold)).size === values.length;
}

/** Adds `value` to `set`, reporting whether it was absent. */
function insert(set: Set<string>, value: string): boolean {
  if (set.has(value)) return false;
  set.add(value);
  return true;
}

/** Rust's `str::split_once`. */
function splitOnce(value: string, separator: string): [string, string] | undefined {
  const index = value.indexOf(separator);
  return index < 0 ? undefined : [value.slice(0, index), value.slice(index + separator.length)];
}

function own<T>(record: Record<string, T>, key: string): T | undefined {
  return Object.hasOwn(record, key) ? record[key] : undefined;
}

function mapValues<T, U>(record: Record<string, T>, map: (value: T) => U): Record<string, U> {
  return Object.fromEntries(Object.entries(record).map(([key, value]) => [key, map(value)]));
}

function first<T>(items: Iterable<T>, check: (item: T) => Problem): Problem {
  for (const item of items) {
    const problem = check(item);
    if (problem !== undefined) return problem;
  }
  return undefined;
}

function within(label: string, problem: Problem): Problem {
  return problem === undefined ? undefined : `${label}: ${problem}`;
}

function oneOf(value: unknown, options: readonly string[]): boolean {
  return typeof value === "string" && options.includes(value);
}

function isInteger(value: unknown, min: number, max: number): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= min && value <= max;
}

function isStrings(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item) => typeof item === "string");
}

function byteLength(value: string): number {
  return encoder.encode(value).length;
}

function jsonBytes(value: unknown): number {
  return byteLength(JSON.stringify(value));
}

function isRecord(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
