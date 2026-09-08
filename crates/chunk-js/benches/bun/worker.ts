// One deployment worker: loads the engine once, then serves serialized calls.
import { type Call, createEngine, type Kind } from "./engine";

declare const self: Worker & { onmessage: ((e: MessageEvent) => void) | null };
const post = self.postMessage.bind(self);

type Init = { kind: Kind; kib: number; init: "eager" | "declarations" };
let engine: Awaited<ReturnType<typeof createEngine>> | null = null;

self.onmessage = async (event: MessageEvent) => {
  const message = event.data as { init?: Init; call?: Call & { id: number } };
  if (message.init) {
    engine = await createEngine(message.init.kind, message.init.kib, message.init.init);
    post({ ready: true });
    return;
  }
  const call = message.call!;
  try {
    post({ id: call.id, outcome: engine!.call(call) });
  } catch (error) {
    post({ id: call.id, error: String(error) });
  }
};
