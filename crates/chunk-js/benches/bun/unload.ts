// Dynamic load/unload behaviour: module registry retention and vm-context
// collection. Worker teardown is in unload-workers.ts. Run with a fixed CPU set
// away from benchmark cells.
import vm from "node:vm";
import { heapSize, heapStats } from "bun:jsc";
import { asScript, fixture } from "./fixture";

const source = fixture(128, "declarations");
const rss = () => Math.round(process.memoryUsage().rss / 1048576);
const heap = () => Math.round(heapSize() / 1048576);
const gc = () => { Bun.gc(true); Bun.gc(true); };
const report: Record<string, unknown> = { bundleKiB: Math.round(source.length / 1024) };

// 1. Distinct module instances via new blob URLs, then drop every reference.
{
  gc();
  const before = { rss: rss(), heap: heap() };
  const count = 300;
  for (let i = 0; i < count; i++) {
    const url = URL.createObjectURL(new Blob([source + `\nexport const v = ${i};`], { type: "application/javascript" }));
    const ns = await import(url);
    if (ns.v !== i) throw new Error("bad module");
    URL.revokeObjectURL(url);
  }
  const loaded = { rss: rss(), heap: heap() };
  gc();
  await Bun.sleep(200);
  gc();
  console.log(JSON.stringify({ modules: { count, before, loaded, afterGc: { rss: rss(), heap: heap() },
    note: "ESM registry entries are never released; growth persists after GC" } }));
}

// 2. vm contexts that each evaluate the bundle, then drop references.
{
  gc();
  const before = { rss: rss(), heap: heap() };
  const script = new vm.Script(asScript(source));
  const count = 300;
  let keep: unknown[] = [];
  for (let i = 0; i < count; i++) {
    const context = vm.createContext({});
    keep.push(script.runInContext(context));
  }
  const loaded = { rss: rss(), heap: heap() };
  keep = [];
  gc();
  await Bun.sleep(200);
  gc();
  console.log(JSON.stringify({ vmContexts: { count, before, loaded, afterGc: { rss: rss(), heap: heap() } } }));
}
