use super::{error, sources::Source};
use crate::project::Inventory;
use crate::quote;
use rolldown::plugin::{
    HookBuildEndArgs, HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookUsage, Plugin, PluginContext, SharedLoadPluginContext,
};
use rolldown::{
    Bundler, BundlerOptions, CodeSplittingMode, OutputFormat, Platform, RawMinifyOptions, RawMinifyOptionsDetailed,
    SourceMapType,
};
use rolldown_common::Output;
use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
const ENTRY: &str = "\0chunk-entry";

/// A migration source as verified against its hash; the bundle uses `code` instead of rereading `path`.
pub(super) struct MigrationSource {
    pub id: String,
    pub path: PathBuf,
    pub code: String,
}

#[derive(Debug)]
struct Boundary {
    entry: Option<String>,
    /// Migration code by path. Migrations may import only `#chunk`, which resolves to `chunk`.
    migrations: BTreeMap<String, String>,
    chunk: String,
    exports: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
}
impl Plugin for Boundary {
    fn name(&self) -> Cow<'static, str> {
        "chunk-boundary".into()
    }
    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load | HookUsage::BuildEnd
    }
    fn build_end(
        &self,
        ctx: &PluginContext,
        _: Option<&HookBuildEndArgs<'_>>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        std::future::ready((|| {
            let mut exports = self.exports.lock().map_err(|_| anyhow::anyhow!("export discovery poisoned"))?;
            for id in ctx.get_module_ids() {
                if let Some(module) = ctx.get_module_info(&id)
                    && module.is_entry
                {
                    exports.insert(id.to_string(), module.exports.iter().map(ToString::to_string).collect());
                }
            }
            Ok(())
        })())
    }
    fn resolve_id(
        &self,
        _: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl Future<Output = HookResolveIdReturn> + Send {
        std::future::ready((|| {
            let source = args.specifier;
            if source == ENTRY {
                return Ok(Some(HookResolveIdOutput::from_id(ENTRY)));
            }
            if let Some(importer) = args.importer
                && self.migrations.contains_key(importer)
            {
                if source == "#chunk" {
                    return Ok(Some(HookResolveIdOutput::from_id(self.chunk.as_str())));
                }
                anyhow::bail!("{importer} imports {source}; migrations may import only from #chunk");
            }
            let scheme = source
                .split_once(':')
                .is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphabetic()));
            if source.starts_with("node:")
                || nodejs_built_in_modules::is_nodejs_builtin_module(source)
                || (scheme && !Path::new(source).is_absolute())
                || Path::new(source).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("node"))
            {
                anyhow::bail!("Unsupported transactional import: {source}");
            }
            Ok(None)
        })())
    }
    fn load(&self, _: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> impl Future<Output = HookLoadReturn> + Send {
        std::future::ready((|| {
            if args.id == ENTRY {
                return Ok(self
                    .entry
                    .as_ref()
                    .map(|code| HookLoadOutput { code: code.as_str().into(), ..Default::default() }));
            }
            Ok(self
                .migrations
                .get(args.id)
                .map(|code| HookLoadOutput { code: code.as_str().into(), ..Default::default() }))
        })())
    }
}
fn options(root: &Path, input: Vec<String>) -> BundlerOptions {
    BundlerOptions {
        input: Some(input.into_iter().map(Into::into).collect()),
        cwd: Some(root.into()),
        platform: Some(Platform::Neutral),
        format: Some(OutputFormat::Esm),
        ..Default::default()
    }
}

pub(super) async fn build(
    root: &Path,
    output: &Path,
    sdk: &Path,
    files: &[Source<'_>],
    migrations: &[MigrationSource],
    inventory: &Inventory,
) -> io::Result<()> {
    let entries: Vec<_> = files.iter().filter(|source| !source.path.to_string_lossy().ends_with(".d.ts")).collect();
    let discovered_exports = Arc::new(Mutex::new(BTreeMap::new()));
    let mut discovery = Bundler::with_plugins(
        options(root, entries.iter().map(|source| source.path.to_string_lossy().into_owned()).collect()),
        vec![Arc::new(Boundary {
            entry: None,
            migrations: BTreeMap::new(),
            chunk: String::new(),
            exports: discovered_exports.clone(),
        })],
    )
    .map_err(error)?;
    let discovered = discovery.generate().await;
    discovery.close().await.map_err(error)?;
    let discovered = discovered.map_err(error)?;
    if let Some(warning) = discovered.warnings.first() {
        return Err(error(warning));
    }
    let modules = std::mem::take(&mut *discovered_exports.lock().map_err(error)?);
    let mut source = entry_source(root, sdk, &entries, &modules, inventory)?;
    migration_source(sdk, migrations, &mut source)?;
    let mut config = options(root, vec![ENTRY.into()]);
    config.dir = Some(output.to_string_lossy().into_owned());
    config.entry_filenames = Some("source.mjs".to_string().into());
    config.code_splitting = Some(CodeSplittingMode::Bool(false));
    config.sourcemap = Some(SourceMapType::File);
    config.minify = Some(RawMinifyOptions::Object(RawMinifyOptionsDetailed {
        mangle: None,
        mangle_properties: None,
        compress: None,
        remove_whitespace: true,
    }));
    let migration_code =
        migrations.iter().map(|migration| (migration.path.to_string_lossy().into_owned(), migration.code.clone()));
    let chunk = root.join(".chunk/generated/index.ts").to_string_lossy().into_owned();
    let boundary =
        Boundary { entry: Some(source), migrations: migration_code.collect(), chunk, exports: Arc::default() };
    let mut bundler = Bundler::with_plugins(config, vec![Arc::new(boundary)]).map_err(error)?;
    let result = bundler.generate().await;
    bundler.close().await.map_err(error)?;
    let result = result.map_err(error)?;
    if let Some(warning) = result.warnings.first() {
        return Err(error(warning));
    }
    write_output(&result.assets, root, output)
}

fn write_output(assets: &[Output], root: &Path, output: &Path) -> io::Result<()> {
    let chunks: Vec<_> =
        assets.iter().filter_map(|asset| if let Output::Chunk(chunk) = asset { Some(chunk) } else { None }).collect();
    if chunks.len() != 1 || !chunks[0].imports.is_empty() || !chunks[0].dynamic_imports.is_empty() {
        return Err(error("Backend must be one self-contained module"));
    }
    let chunk = chunks[0];
    let mut map: serde_json::Value =
        serde_json::from_str(&chunk.map.as_ref().ok_or_else(|| error("missing source map"))?.to_json_string())
            .map_err(error)?;
    if let Some(sources) = map["sources"].as_array_mut() {
        for source in sources {
            if let Some(name) = source.as_str() {
                let path = output.join(name).canonicalize().unwrap_or_else(|_| output.join(name));
                let relative = if let Ok(relative) = path.strip_prefix(root) {
                    relative.to_string_lossy().into_owned()
                } else {
                    name.into()
                };
                *source = staged_migration(&relative.replace('\\', "/")).into();
            }
        }
    }
    fs::write(output.join("source.mjs"), &chunk.code)?;
    fs::write(output.join("source.mjs.map"), serde_json::to_vec(&map).map_err(error)?)?;
    Ok(())
}

/// Names a migration staged at `.chunk/compile-*/migrations/<id>.ts` by its place in the project.
fn staged_migration(path: &str) -> String {
    match path.strip_prefix(".chunk/compile-").and_then(|rest| rest.split_once("/migrations/")) {
        Some((_, file)) => format!("server/migrations/{file}"),
        None => path.to_owned(),
    }
}

fn entry_source(
    root: &Path,
    sdk: &Path,
    entries: &[&Source<'_>],
    modules: &BTreeMap<String, Vec<String>>,
    inventory: &Inventory,
) -> io::Result<String> {
    use std::fmt::Write;
    let mut source = format!(
        "import schema from {};\nimport {{ isHook, invokeHook }} from {};\nimport {{ isCommand, invokeCommand }} from {};\n",
        quote(root.join("server/schema/index.ts").to_string_lossy()),
        quote(sdk.join("hooks.ts").to_string_lossy()),
        quote(sdk.join("commands.ts").to_string_lossy()),
    );
    let mut descriptors = super::descriptors::Descriptors::new(sdk, &mut source)?;
    writeln!(
        source,
        "import {{isApp,isScope,appConfigurations,appDestinations}} from {};",
        quote(sdk.join("apps.ts").to_string_lossy())
    )
    .map_err(error)?;
    let mut domains = super::domains::DomainEntries::new(inventory);
    let mut bindings = 0;
    for (index, entry) in entries.iter().enumerate() {
        writeln!(source, "import * as m{index} from {};", quote(entry.path.to_string_lossy())).map_err(error)?;
        descriptors.add_module(entry)?;
        if let Some(module) = entry.authoring {
            let value = format!("m{index}.default");
            domains.add_authored(module, &value, &mut source)?;
            if module.app {
                let app = inventory
                    .apps
                    .iter()
                    .find(|app| entry.namespace == format!("apps/{}/app", app.id))
                    .ok_or_else(|| error("missing authored app metadata"))?;
                writeln!(
                    source,
                    "if({value}.id !== {}) throw new Error('App id differs from statically discovered metadata');",
                    quote(&app.id)
                )
                .map_err(error)?;
                descriptors.add_app(&value, app);
            }
        }
        let key = entry.path.to_string_lossy();
        let module = modules.get(key.as_ref()).ok_or_else(|| error(format!("missing module exports: {key}")))?;
        let mut exports = module.clone();
        exports.sort();
        for exported in exports {
            if exported == "*" {
                return Err(error(format!("Star re-exports are unsupported: {key}")));
            }
            let value = format!("m{index}[{}]", quote(&exported));
            let binding = format!("f{bindings}");
            domains.add_export(entry, &exported, &value, &mut source)?;
            if exported == "default" {
                descriptors.add_default(entry, &value, &mut source)?;
                continue;
            }
            writeln!(source, "export const {binding} = (ctx,args) => isHook({value}) ? invokeHook({value},ctx,args) : isCommand({value}) ? invokeCommand({value},ctx,args) : {value}.handler(ctx,args);").map_err(error)?;
            bindings += 1;
            descriptors.add_export(entry, &exported, &value, &binding, &mut source)?;
        }
    }
    source.push_str("if (schema === null || typeof schema !== 'object' || schema.contract === null || typeof schema.contract !== 'object' || Array.isArray(schema.contract)) throw new Error('server/schema/index.ts must default-export a schema created with defineSchema()');\n");
    domains.bindings(&mut source)?;
    let domain_metadata = domains.metadata()?;
    let super::descriptors::Metadata { functions, methods, destinations, configurations } = descriptors.metadata();
    writeln!(source, "const destinationEntries = [{destinations}];").map_err(error)?;
    write!(source, "export function __chunk_contract() {{ const methods = [{methods}]; const configurations = [{configurations}]; return {{contract_version:{},runtime_profile:'transactional_v1',tables:schema.contract,functions:Object.fromEntries([{functions}]){domain_metadata},...(destinationEntries.length ? {{destinations:{{version:1,entries:Object.fromEntries(destinationEntries)}}}} : {{}}),...(methods.length ? {{session_methods:{{version:1,methods}}}} : {{}}),...(configurations.length ? {{session_configurations:{{version:1,configurations}}}} : {{}})}}; }}", chunk_contract::CONTRACT_VERSION).map_err(error)?;
    Ok(source)
}

/// Registers each migration by ID behind `__chunk_migrate`, and reports which tables have `back` through
/// `__chunk_migrations`.
fn migration_source(sdk: &Path, migrations: &[MigrationSource], source: &mut String) -> io::Result<()> {
    use std::fmt::Write;
    writeln!(
        source,
        "import {{ isMigration, migrate, migrationBacks }} from {};\nconst migrations = {{}};",
        quote(sdk.join("migrations.ts").to_string_lossy())
    )
    .map_err(error)?;
    for (index, MigrationSource { id, path, .. }) in migrations.iter().enumerate() {
        let message = format!("server/migrations/{id}.ts must default-export defineMigration({})", quote(id));
        writeln!(
            source,
            "import mg{index} from {};\nif (!isMigration(mg{index}) || mg{index}.id !== {}) throw new Error({});\nmigrations[{}] = mg{index};",
            quote(path.to_string_lossy()),
            quote(id),
            quote(message),
            quote(id)
        )
        .map_err(error)?;
    }
    source.push_str("export const __chunk_migrate = (_, args) => migrate(migrations, args);\nexport const __chunk_migrations = () => migrationBacks(migrations);\n");
    Ok(())
}
