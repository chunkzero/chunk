import { fixture } from "./fixture";
const source = fixture(128, "declarations");
const rss = () => Math.round(process.memoryUsage().rss / 1048576);
const url = URL.createObjectURL(new Blob([source + `\nself.onmessage = () => {};\nself.postMessage("ready");`], { type: "application/javascript" }));
const count = 40;
const before = { rss: rss() };
const workers: Worker[] = [];
const started = performance.now();
for (let i = 0; i < count; i++) {
  const worker = new Worker(url, { type: "module" });
  await new Promise((resolve, reject) => {
    worker.addEventListener("message", resolve, { once: true });
    worker.addEventListener("error", (e) => reject(new Error((e as ErrorEvent).message)), { once: true });
  });
  workers.push(worker);
}
const loaded = { rss: rss(), msPerWorker: Math.round((performance.now() - started) / count * 100) / 100 };
await Promise.all(workers.map((worker) => new Promise<void>((resolve) => { worker.addEventListener("close", () => resolve(), { once: true }); worker.terminate(); })));
Bun.gc(true);
await Bun.sleep(500);
console.log(JSON.stringify({ workers: { count, before, loaded, afterTerminate: { rss: rss() } } }));
const code = `import { heapSize } from "bun:jsc";
  const a = []; let stopped = false;
  for (let i = 0; i < 1e6; i++) { a.push(new Array(1000).fill(i)); if (i % 100 === 0 && heapSize() > 64 * 1048576) { stopped = true; break; } }
  self.postMessage({ stopped, heapMiB: Math.round(heapSize() / 1048576), arrays: a.length });`;
const worker = new Worker(URL.createObjectURL(new Blob([code], { type: "application/javascript" })), { type: "module" });
console.log(JSON.stringify({ selfLimit: await new Promise((resolve, reject) => { worker.addEventListener("message", (e) => resolve((e as MessageEvent).data), { once: true }); worker.addEventListener("error", (e) => reject(new Error((e as ErrorEvent).message)), { once: true }); }) }));
worker.terminate();
process.exit(0);
