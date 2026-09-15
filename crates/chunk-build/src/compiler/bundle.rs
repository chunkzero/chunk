use super::sources::Source;
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
    path::Path,
    sync::{Arc, Mutex},
};
const ENTRY: &str = "\0chunk-entry";

#[derive(Debug)]
struct Boundary {
    entry: Option<String>,
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
            Ok(None)
        })())
    }
}
fn error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
fn quote(value: impl AsRef<str>) -> String {
    serde_json::to_string(value.as_ref()).expect("string serialization")
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

pub(super) async fn build(root: &Path, output: &Path, sdk: &Path, files: &[Source]) -> io::Result<()> {
    let entries: Vec<_> = files.iter().filter(|source| !source.path.to_string_lossy().ends_with(".d.ts")).collect();
    let discovered_exports = Arc::new(Mutex::new(BTreeMap::new()));
    let mut discovery = Bundler::with_plugins(
        options(root, entries.iter().map(|source| source.path.to_string_lossy().into_owned()).collect()),
        vec![Arc::new(Boundary { entry: None, exports: discovered_exports.clone() })],
    )
    .map_err(error)?;
    let discovered = discovery.generate().await;
    discovery.close().await.map_err(error)?;
    let discovered = discovered.map_err(error)?;
    if let Some(warning) = discovered.warnings.first() {
        return Err(error(warning));
    }
    let modules = std::mem::take(&mut *discovered_exports.lock().map_err(error)?);
    let source = entry_source(root, sdk, &entries, &modules)?;
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
    let mut bundler =
        Bundler::with_plugins(config, vec![Arc::new(Boundary { entry: Some(source), exports: Arc::default() })])
            .map_err(error)?;
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
                *source = relative.replace('\\', "/").into();
            }
        }
    }
    fs::write(output.join("source.mjs"), &chunk.code)?;
    fs::write(output.join("source.mjs.map"), serde_json::to_vec(&map).map_err(error)?)?;
    Ok(())
}

fn entry_source(
    root: &Path,
    sdk: &Path,
    entries: &[&Source],
    modules: &BTreeMap<String, Vec<String>>,
) -> io::Result<String> {
    use std::fmt::Write;
    let mut source = format!(
        "import schema from {};\nimport {{ isFunction }} from {};\nimport {{ isHook, invokeHook }} from {};\n",
        quote(root.join("server/schema/index.ts").to_string_lossy()),
        quote(sdk.join("functions.ts").to_string_lossy()),
        quote(sdk.join("hooks.ts").to_string_lossy())
    );
    let domains = super::domains::manifest(root)?;
    let mut hook_metadata = Vec::new();
    let mut hook_scopes = std::collections::BTreeSet::new();
    let mut metadata = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        writeln!(source, "import * as m{index} from {};", quote(entry.path.to_string_lossy())).map_err(error)?;
        let scope = super::domains::hook_scope(entry);
        if scope.is_some_and(|scope| !hook_scopes.insert(scope)) {
            return Err(error(format!("multiple hook modules for domain {}", scope.unwrap_or_default())));
        }
        let key = entry.path.to_string_lossy();
        let location = entry.path.strip_prefix(root).unwrap_or(&entry.path).to_string_lossy();
        let module = modules.get(key.as_ref()).ok_or_else(|| error(format!("missing module exports: {key}")))?;
        let mut exports = module.clone();
        exports.sort();
        for exported in exports {
            if exported == "*" {
                return Err(error(format!("Star re-exports are unsupported: {key}")));
            }
            let value = format!("m{index}[{}]", quote(&exported));
            if exported == "default" {
                writeln!(
                    source,
                    "if(isHook({value})) throw new Error({});",
                    quote(format!("Hook descriptors require named exports: {location}"))
                )
                .map_err(error)?;
                continue;
            }
            let name = quote(format!("{}/{}", entry.namespace, exported));
            let binding = format!("f{}", metadata.len());
            writeln!(source, "export const {binding} = (ctx,args) => isHook({value}) ? invokeHook({value},ctx,args) : {value}.handler(ctx,args);").map_err(error)?;
            if let Some(scope) = scope {
                hook_metadata.push(format!(
                    "...(isHook({value}) ? [[{name}, {{...{value}.contract, domain:{}, export:{}}}]] : [])",
                    quote(scope),
                    quote(&binding)
                ));
            } else {
                writeln!(
                    source,
                    "if(isHook({value})) throw new Error({});",
                    quote(format!(
                        "Hook descriptors must be named exports in server/domains/**/hooks.ts or hooks.mts: {location}"
                    ))
                )
                .map_err(error)?;
            }
            metadata.push(format!(
                "...(isFunction({value}) ? [[{name}, {{...{value}.contract, export:{}}}]] : [])",
                quote(binding)
            ));
        }
    }
    source.push_str("if (schema === null || typeof schema !== 'object' || schema.contract === null || typeof schema.contract !== 'object' || Array.isArray(schema.contract)) throw new Error('server/schema/index.ts must default-export a schema created with defineSchema()');\n");
    let domain_metadata = if let Some(domains) = domains {
        format!(
            ",domains:{{...{},hooks:Object.fromEntries([{}])}}",
            serde_json::to_string(&domains).map_err(error)?,
            hook_metadata.join(",")
        )
    } else {
        String::new()
    };
    write!(source, "export function __chunk_contract() {{ return {{contract_version:2,runtime_profile:'transactional_v1',tables:schema.contract,functions:Object.fromEntries([{}]){domain_metadata}}}; }}", metadata.join(",")).map_err(error)?;
    Ok(source)
}
