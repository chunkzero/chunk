export type Doc = { score: number; name: string; online: boolean };
export type Rows = Record<string, Doc>;
export type Read =
  | { kind: "get"; table: string; id: string }
  | { kind: "scan"; table: string; start: string | null; end: string | null };
export type Write = { key: { table: string; id: string }; value: Doc | null };
export type Deps = { points: string[]; scans: [string, string | null, string | null][] };

export const pad = (i: number) => String(i).padStart(4, "0");

// Same rows as the Rust harness: plain and zero-padded ids per revision.
export function revisionRows(revision: number): Rows {
  const rows: Rows = {};
  for (let i = 0; i < 20; i++) {
    const value = { score: i + (revision % 7), name: `player-${i}`, online: true };
    rows[String(i)] = { ...value };
    rows[pad(i)] = { ...value };
  }
  return rows;
}

// In-memory snapshot host with dependency recording and speculative writes.
export class Snapshot {
  points = new Set<string>();
  scans: Deps["scans"] = [];
  writes: Write[] = [];
  constructor(readonly rows: Rows) {}
  read(read: Read) {
    if (read.table !== "players") throw new Error("unknown table");
    if (read.kind === "get") {
      this.points.add(read.id);
      return this.rows[read.id] ?? null;
    }
    this.scans.push([read.table, read.start, read.end]);
    return Object.keys(this.rows).sort()
      .filter(id => (read.start === null || id >= read.start) && (read.end === null || id < read.end))
      .map(id => [id, this.rows[id]]);
  }
  write(key: Write["key"], value: Doc | null) {
    this.writes.push({ key, value });
  }
  deps(): Deps {
    return { points: [...this.points], scans: this.scans };
  }
}
