import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const directory = mkdtempSync(join(tmpdir(), "chunk-api-"));
const target = "apps/dashboard/src/lib/api.generated.ts";
try {
    const schema = join(directory, "openapi.json");
    const output = join(directory, "api.ts");
    writeFileSync(schema, execFileSync("cargo", ["run", "--quiet", "-p", "chunk-management", "--example", "openapi"]));
    execFileSync("pnpm", ["exec", "openapi-typescript", schema, "-o", output], { stdio: "inherit" });
    execFileSync("pnpm", ["exec", "oxfmt", output], { stdio: "inherit" });
    const generated = readFileSync(output, "utf8");
    if (process.argv.includes("--check")) {
        if (readFileSync(target, "utf8") !== generated) throw new Error("API bindings are stale. Run pnpm api:generate.");
    } else writeFileSync(target, generated);
} finally {
    rmSync(directory, { recursive: true, force: true });
}
