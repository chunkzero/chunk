import fs from "node:fs/promises"
import path from "node:path"
import { builtinModules } from "node:module"
import { fileURLToPath } from "node:url"
import { execFileSync } from "node:child_process"
import { rolldown } from "rolldown"

const sdk = fileURLToPath(new URL("../server/src/index.ts", import.meta.url))
const forbidden = new Set(builtinModules.map(name => name.replace(/^node:/, "")))

async function sources(directory, files = []) {
  let entries
  try { entries = await fs.readdir(directory, { withFileTypes: true }) }
  catch (error) { if (error.code === "ENOENT") return files; throw error }
  for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name, "en"))) {
    if (["node_modules", "_generated"].includes(entry.name)) continue
    const filename = path.join(directory, entry.name)
    if (entry.isSymbolicLink()) throw new Error(`Source symlinks are unsupported: ${filename}`)
    if (entry.isDirectory()) await sources(filename, files)
    else if (entry.isFile() && /\.(?:ts|mts)$/.test(entry.name)) files.push(filename)
    if (files.length > 512) throw new Error("Too many backend source files")
  }
  return files
}

async function compile(root, output) {
  root = await fs.realpath(root)
  const shared = await sources(path.join(root, "server"))
  const files = shared.map(file => ({ file, namespace: `shared/${path.relative(path.join(root, "server"), file).replace(/\.(?:ts|mts)$/, "")}` }))
  let apps = []
  try { apps = await fs.readdir(path.join(root, "apps"), { withFileTypes: true }) }
  catch (error) { if (error.code !== "ENOENT") throw error }
  for (const app of apps.sort((a, b) => a.name.localeCompare(b.name, "en"))) {
    if (app.isSymbolicLink()) throw new Error(`App symlinks are unsupported: ${app.name}`)
    if (!app.isDirectory()) continue
    const directory = path.join(root, "apps", app.name, "server")
    for (const file of await sources(directory)) files.push({ file, namespace: `apps/${app.name}/${path.relative(directory, file).replace(/\.(?:ts|mts)$/, "")}` })
  }
  if (files.length > 512) throw new Error("Too many backend source files")
  const total = (await Promise.all(files.map(({ file }) => fs.stat(file)))).reduce((n, stat) => n + stat.size, 0)
  if (total > 8 * 1024 * 1024) throw new Error("Backend source size limit")
  const schema = path.join(root, "server/schema/index.ts")
  if (!files.some(({ file }) => file === schema)) throw new Error("Missing explicitly composed server/schema/index.ts")
  await fs.mkdir(output, { recursive: true })
  const config = path.join(output, "tsconfig.json")
  await fs.writeFile(config, JSON.stringify({
    compilerOptions: {
      target: "ES2023", module: "ESNext", moduleResolution: "Bundler",
      strict: true, exactOptionalPropertyTypes: true, noEmit: true, allowImportingTsExtensions: true,
      paths: { "@chunk/server": [sdk] }, types: [], lib: ["ES2023"],
    }, files: files.map(({ file }) => file),
  }))
  const tsc = path.join(path.dirname(fileURLToPath(import.meta.resolve("typescript/package.json"))), "bin/tsc")
  try { execFileSync(process.execPath, [tsc, "--project", config], { encoding: "utf8", maxBuffer: 1024 * 1024 }) }
  catch (error) { throw new Error(error.stdout || error.message) }
  const virtual = "\0chunk-entry"
  const boundary = {
    name: "chunk-boundary", resolveId(source) {
      if (source === virtual) return source
      if (source === "@chunk/server") return sdk
      if (source.startsWith("node:") || forbidden.has(source) || /^[a-z]+:/i.test(source) || source.endsWith(".node")) {
        throw new Error(`Unsupported transactional import: ${source}`)
      }
    },
  }
  const entries = files.filter(({ file }) => !file.endsWith(".d.ts"))
  const exports = new Map()
  const discovery = await rolldown({
    input: entries.map(({ file }) => file), cwd: root, platform: "neutral",
    plugins: [boundary, { name: "chunk-exports", buildEnd() {
      for (const { file } of entries) exports.set(file, this.getModuleInfo(file)?.exports ?? [])
    } }],
    onwarn(warning) { if (warning.code !== "EMPTY_BUNDLE") throw new Error(warning.message) },
  })
  try { await discovery.generate({ format: "esm" }) } finally { await discovery.close() }
  const imports = [`import schema from ${JSON.stringify(schema)};`, `import { isFunction } from ${JSON.stringify(sdk)};`]
  const bindings = []
  const metadata = []
  for (const [index, entry] of entries.entries()) {
    const module = `m${index}`
    imports.push(`import * as ${module} from ${JSON.stringify(entry.file)};`)
    for (const exported of exports.get(entry.file)) {
      if (exported === "*") throw new Error(`Star re-exports are unsupported: ${entry.file}`)
      if (exported === "default") continue
      const name = `${entry.namespace}/${exported}`.replaceAll(path.sep, "/")
      const binding = `f${bindings.length}`
      const value = `${module}[${JSON.stringify(exported)}]`
      bindings.push(`export const ${binding} = (ctx, args) => ${value}.handler(ctx, args);`)
      metadata.push(`...(isFunction(${value}) ? [[${JSON.stringify(name)}, {...${value}.contract, export:${JSON.stringify(binding)}}]] : [])`)
    }
  }
  const entry = `${imports.join("\n")}\n${bindings.join("\n")}\nexport function __chunk_contract() { return {contract_version:1,runtime_profile:"transactional_v1",tables:schema.contract,functions:Object.fromEntries([${metadata.join(",")}])}; }`
  const bundle = await rolldown({
    input: virtual, cwd: root, platform: "neutral",
    plugins: [boundary, { name: "chunk-entry", load(id) { if (id === virtual) return entry } }],
    onwarn(warning) { throw new Error(warning.message) },
  })
  try {
    const result = await bundle.generate({
      format: "esm", codeSplitting: false, sourcemap: true, dir: output, entryFileNames: "source.mjs",
      minify: { mangle: false, compress: false, codegen: { removeWhitespace: true } },
      sourcemapPathTransform: source => {
        const absolute = path.resolve(output, source)
        for (const [base, prefix] of [[path.dirname(sdk), "@chunk/server"], [root, ""]]) {
          if (absolute.startsWith(base + path.sep)) return path.posix.join(prefix, ...path.relative(base, absolute).split(path.sep))
        }
        return source
      },
    })
    const chunks = result.output.filter(output => output.type === "chunk")
    if (chunks.length !== 1 || chunks[0].imports.length) throw new Error("Backend must be one self-contained module")
    const chunk = chunks[0]
    await fs.mkdir(output, { recursive: true })
    await fs.writeFile(path.join(output, "source.mjs"), chunk.code)
    await fs.writeFile(path.join(output, "source.mjs.map"), chunk.map.toString())
  } finally { await bundle.close() }
}

const [root, output] = process.argv.slice(2)
if (!root || !output) throw new Error("usage: bundle.mjs PROJECT OUTPUT")
await compile(root, output)
