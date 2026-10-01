import { v } from "./validators.ts";
import type { Destination, PlayerId } from "./validators.ts";

/** Why a move was refused, which queued nothing. */
export type MoveRefusal = "offline" | "stale" | "full" | "unknown_destination";
/** Acceptance means the player's gateway carries the move out, not that the player arrived. */
export type MoveResult =
  | { readonly state: "accepted"; readonly operationId: string }
  | { readonly state: "refused"; readonly reason: MoveRefusal };

export interface ActionRouting {
  /** Requests a move of any player in the environment through the destination's admission policy. */
  move(player: PlayerId, destination: Destination): Promise<MoveResult>;
}

const accepted = v.object({ state: v.literal("accepted"), operationId: v.string() });
const refused = v.object({
  state: v.literal("refused"),
  reason: v.enum("offline", "stale", "full", "unknown_destination"),
});

export function actionRouting(platform: (request: unknown) => Promise<unknown>): ActionRouting {
  return Object.freeze({
    move: async (player: PlayerId, destination: Destination): Promise<MoveResult> => {
      const request = {
        kind: "move",
        player: v.player().parse(player),
        destination: v.destination().parse(destination),
      };
      const result = await platform(request);
      return (result as { state?: unknown } | null)?.state === "refused"
        ? refused.parse(result)
        : accepted.parse(result);
    },
  });
}
