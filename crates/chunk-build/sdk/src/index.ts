/// <reference path="./web.d.ts" />
export { v } from "./validators.ts";
export type {
  Validator,
  ObjectValidator,
  OptionalValidator,
  Shape,
  Infer,
  InferObject,
  Id,
  PlayerIdentity,
  Destination,
  AdmissionResult,
  ServerStatus,
  PlayerId,
  SessionId,
  JsonValue,
} from "./validators.ts";
export { defineTable, defineSchema } from "./schema.ts";
export type { TableDefinition, SchemaDefinition } from "./schema.ts";
export {
  action,
  internalAction,
  query,
  mutation,
  internalQuery,
  internalMutation,
  isFunction,
  defineFunctions,
} from "./functions.ts";
export type {
  AsyncContext,
  ActionContext,
  HttpRequest,
  HttpOutcome,
  FunctionDefinition,
  FunctionBuilder,
  QueryContext,
  MutationContext,
  FunctionReference,
} from "./functions.ts";
export { unset } from "./documents.ts";
export type { Document, Reader, Writer, Selection } from "./documents.ts";

export { createHook } from "./hooks.ts";
export type { HookContexts, HookResults, HookEvent, HookOptions, HookDefinition } from "./hooks.ts";

export { command, commandRoute, commandArg } from "./commands.ts";
export type {
  CommandContext,
  CommandArgument,
  CommandArguments,
  CommandShape,
  CommandRoute,
  CommandDefinition,
  CommandPermission,
  CommandSuggestions,
  SuggestionQuery,
} from "./commands.ts";

export { sessionMethod } from "./sessions.ts";
export type { SessionMethodDeclaration, SessionMethodReference } from "./sessions.ts";
