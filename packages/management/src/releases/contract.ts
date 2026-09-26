/**
 * The shape of `contract.json` as chunk_contract deserializes it (crates/chunk-contract/src/deployment.rs and
 * schema.rs): unknown fields are refused, schemas are tagged by `type`, and nesting and counts are limited.
 */

const contractVersion = 2;
const runtimeProfiles = ["transactional_v1"];
const optionalContracts = ["domains", "session_methods", "session_configurations", "destinations"];
const maxTables = 128;
const maxFunctions = 256;
const maxDepth = 32;
const functionKinds = ["query", "mutation", "action"];
const visibilities = ["public", "internal"];

/** Describes the first way `contract` differs from compiled backend metadata, or undefined when it is some. */
export function contractProblem(contract: unknown): string | undefined {
  if (!isRecord(contract)) return "is not a JSON object";
  const unexpected = unknownKey(
    contract,
    ["contract_version", "runtime_profile", "tables", "functions"],
    optionalContracts,
  );
  if (unexpected !== undefined) return `has an unexpected field ${unexpected}`;
  if (contract.contract_version !== contractVersion) return `is not contract version ${contractVersion}`;
  if (!runtimeProfiles.includes(contract.runtime_profile as string)) return "has an unknown runtime_profile";
  for (const field of optionalContracts) {
    if (Object.hasOwn(contract, field) && !isRecord(contract[field])) return `has an invalid ${field}`;
  }
  const { tables, functions } = contract;
  if (!isRecord(tables) || Object.keys(tables).length > maxTables) return "has invalid tables";
  for (const [name, table] of Object.entries(tables)) {
    const problem = tableProblem(table);
    if (problem !== undefined) return `has an invalid table ${name}: ${problem}`;
  }
  if (!isRecord(functions) || Object.keys(functions).length > maxFunctions) return "has invalid functions";
  for (const [path, declaration] of Object.entries(functions)) {
    const problem = functionProblem(declaration);
    if (problem !== undefined) return `has an invalid function ${path}: ${problem}`;
  }
  return undefined;
}

function tableProblem(table: unknown): string | undefined {
  if (!isRecord(table) || unknownKey(table, ["fields"], ["indexes"]) !== undefined) return "is not a table";
  const fields = fieldsProblem(table.fields, 0);
  if (fields !== undefined) return fields;
  if (Object.hasOwn(table, "indexes")) {
    const { indexes } = table;
    if (!isRecord(indexes)) return "has invalid indexes";
    for (const index of Object.values(indexes)) {
      if (!Array.isArray(index) || !index.every((field) => typeof field === "string")) return "has invalid indexes";
    }
  }
  return undefined;
}

function functionProblem(declaration: unknown): string | undefined {
  if (
    !isRecord(declaration) ||
    unknownKey(declaration, ["kind", "visibility", "export", "arguments", "result"]) !== undefined
  ) {
    return "is not a function declaration";
  }
  if (!functionKinds.includes(declaration.kind as string)) return "has an unknown kind";
  if (!visibilities.includes(declaration.visibility as string)) return "has an unknown visibility";
  if (typeof declaration.export !== "string") return "has no export";
  return schemaProblem(declaration.arguments, 0) ?? schemaProblem(declaration.result, 0);
}

function fieldsProblem(fields: unknown, depth: number): string | undefined {
  if (!isRecord(fields)) return "has no fields";
  for (const [name, field] of Object.entries(fields)) {
    if (!isRecord(field) || unknownKey(field, ["schema"], ["optional"]) !== undefined)
      return `has an invalid field ${name}`;
    if (Object.hasOwn(field, "optional") && typeof field.optional !== "boolean") return `has an invalid field ${name}`;
    const problem = schemaProblem(field.schema, depth + 1);
    if (problem !== undefined) return problem;
  }
  return undefined;
}

/** Checks one `Schema`, a JSON object tagged by `type` whose other keys depend on the tag. */
function schemaProblem(schema: unknown, depth: number): string | undefined {
  if (depth > maxDepth) return "nests schemas too deeply";
  if (!isRecord(schema) || typeof schema.type !== "string") return "has a schema without a type";
  const only = (...keys: string[]) =>
    unknownKey(schema, ["type", ...keys]) === undefined ? undefined : `has an invalid ${schema.type} schema`;
  switch (schema.type) {
    case "null":
    case "boolean":
    case "number":
    case "integer":
    case "string":
    case "player":
    case "session":
      return only();
    case "id":
      return only("table") ?? (typeof schema.table === "string" ? undefined : "has an id schema without a table");
    case "literal":
      return only("value");
    case "enum":
      return (
        only("values") ??
        (Array.isArray(schema.values) && schema.values.every((value) => typeof value === "string")
          ? undefined
          : "has an enum schema with invalid values")
      );
    case "nullable":
      return only("value") ?? schemaProblem(schema.value, depth + 1);
    case "array":
      return only("items") ?? schemaProblem(schema.items, depth + 1);
    case "object":
      return only("fields") ?? fieldsProblem(schema.fields, depth);
    case "union": {
      const invalid = only("variants");
      if (invalid !== undefined) return invalid;
      if (!isRecord(schema.variants)) return "has a union schema without variants";
      for (const variant of Object.values(schema.variants)) {
        const problem = schemaProblem(variant, depth + 1);
        if (problem !== undefined) return problem;
      }
      return undefined;
    }
    default:
      return `has an unknown schema type ${JSON.stringify(schema.type)}`;
  }
}

/** The first key of `value` outside `required` and `optional`, or `required`'s first missing key. */
function unknownKey(value: Record<string, unknown>, required: string[], optional: string[] = []): string | undefined {
  const missing = required.find((key) => !Object.hasOwn(value, key));
  if (missing !== undefined) return missing;
  return Object.keys(value).find((key) => !required.includes(key) && !optional.includes(key));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
