import { apiValidator, argumentSchema, freeze, validator } from "./validators.ts";
import type { InferObject, ObjectValidator, Schema, Shape, Validator } from "./validators.ts";

const declaration = Symbol.for("@chunk/session-method");

/** An authored, typed reference to a method implemented by one gameplay session type. */
export interface SessionMethodReference<A, R> {
  readonly app: string;
  readonly session: string;
  readonly name: string;
  readonly arguments: Validator<A>;
  readonly result: Validator<R>;
}

export interface SessionMethodDeclaration<A = never, R = unknown> extends SessionMethodReference<A, R> {
  readonly [declaration]: true;
  readonly contract: { app: string; session: string; name: string; arguments: Schema; result: Schema };
}

/** Declares the wire contract before JVM compilation; no gameplay method runs here. */
export function sessionMethod<const S extends Shape, R>(options: {
  app: string;
  session: string;
  name: string;
  args: S | ObjectValidator<S>;
  returns: Validator<R>;
}): SessionMethodDeclaration<InferObject<S>, R> {
  for (const [kind, value] of Object.entries({ app: options.app, session: options.session, name: options.name })) {
    if (!/^[A-Za-z_][A-Za-z0-9_]{0,127}$/.test(value)) throw new Error(`Invalid session method ${kind}: ${value}`);
  }
  const args = argumentSchema(options.args);
  return freeze({
    [declaration]: true as const,
    app: options.app,
    session: options.session,
    name: options.name,
    arguments: apiValidator(validator<InferObject<S>>(args)),
    result: apiValidator(options.returns),
    contract: {
      app: options.app,
      session: options.session,
      name: options.name,
      arguments: args,
      result: options.returns.schema,
    },
  });
}

export function isSessionMethod(value: unknown): value is SessionMethodDeclaration {
  return value !== null && typeof value === "object" && declaration in value && value[declaration] === true;
}
