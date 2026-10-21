//! `trim_apis`: pi-ai's stream APIs outside `[trim] apis` are stubbed out.
//!
//! In Pi 1.3.0 every stream API is reached through one module, `dist/api/<api>.lazy.js`:
//! `lazyApi(() => import("./<api>.js"))`. Built-in providers and compat's `BUILTIN_APIS` registry
//! (models.json and extension providers) both take their API object from there, so one stub
//! there covers both, and esbuild leaves the API's code and its SDK out. Bedrock is loaded
//! through a variable specifier instead, and pibake's entry hands it `dist/bedrock-provider.js`,
//! which imports the AWS SDK statically; that file is the one stubbed for Bedrock. The model
//! catalog stays whole: a request to a dropped API fails through `lazyStream` with one line.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use oxc::allocator::Allocator;
use oxc::ast::ast::{
    Argument, ArrayExpressionElement, BindingPattern, Declaration, Expression,
    ImportDeclarationSpecifier, Statement,
};
use oxc::span::{GetSpan, SourceType};
use serde_json::Value;

use super::{Change, Frozen, Setup, Sources, parse, range, snippet, splice};
use crate::lock::PI_PACKAGE;

const PI_AI: &str = "@earendil-works/pi-ai";
const COMPAT: &str = "dist/compat.js";
const REGISTRY: &str = "BUILTIN_APIS";
const LAZY_API: &str = "lazyApi";
const BEDROCK: &str = "bedrock-converse-stream";
const BEDROCK_PROVIDER: &str = "dist/bedrock-provider.js";
const BEDROCK_IMPORT: &str = "./api/bedrock-converse-stream.js";
const CATALOG: &str = "dist/providers/data";
const SETTING: &str = "[trim] apis";
const SOURCE_EXTENSIONS: [&str; 8] = ["js", "mjs", "cjs", "ts", "mts", "cts", "tsx", "jsx"];

/// What a request to a dropped API fails with.
pub fn left_out_message(api: &str) -> String {
    format!("the {api} API was left out of this pibake build; add it to {SETTING} in pibake.toml")
}

fn stub(api: &str) -> String {
    format!(
        "() => Promise.reject(new Error({}))",
        Value::from(left_out_message(api))
    )
}

fn bedrock_stub() -> String {
    format!(
        "const stream = () => {{ throw new Error({}); }};\nconst streamSimple = stream;",
        Value::from(left_out_message(BEDROCK))
    )
}

pub struct Out {
    pub changes: Vec<Change>,
    pub notes: Vec<String>,
    pub frozen: Frozen,
}

pub fn run(sources: &mut Sources, explicit: Option<&[String]>, setup: &Setup) -> Result<Out> {
    let pi_ai = sources.resolve_package(&format!("node_modules/{PI_PACKAGE}/dist"), PI_AI)?;
    let compat = format!("{pi_ai}/{COMPAT}");
    let apis = registry(&sources.read(&compat)?, &compat)?;
    let providers = catalog(&sources.stage().join(&pi_ai).join(CATALOG), &apis)?;

    let (kept, notes) = match explicit {
        Some(list) => {
            let unknown: Vec<&str> = list
                .iter()
                .map(String::as_str)
                .filter(|a| !apis.contains_key(*a))
                .collect();
            if !unknown.is_empty() {
                bail!(
                    "{SETTING} names {}, which pi-ai's {REGISTRY} does not register; it knows: {}",
                    unknown.join(", "),
                    names(apis.keys())
                );
            }
            let kept: BTreeSet<String> = list.iter().cloned().collect();
            let note = format!("keeps {} (from {SETTING})", names(kept.iter()));
            (kept, vec![note])
        }
        None => match derive(setup, sources.stage(), &apis, &providers)? {
            Derived::All(why) => (
                apis.keys().cloned().collect(),
                vec![format!("keeps every API: {why}")],
            ),
            Derived::Some(reasons) => {
                let notes = reasons
                    .iter()
                    .map(|(api, why)| format!("keeps {api}: {}", names(why.iter())))
                    .collect();
                (reasons.into_keys().collect(), notes)
            }
        },
    };

    let mut changes = vec![];
    for (api, lazy) in &apis {
        let keep = kept.contains(api);
        let (rel, change) = if api == BEDROCK {
            let rel = format!("{pi_ai}/{BEDROCK_PROVIDER}");
            let text = sources.read(&rel)?;
            let change = stub_bedrock(&text, &rel, keep)?;
            (rel, change)
        } else {
            let rel = format!("{pi_ai}/{}", lazy.file);
            let text = sources.read(&rel)?;
            let change = stub_lazy(&text, &rel, api, &lazy.factory, keep)?;
            (rel, change)
        };
        if let Some(text) = change
            && let Some(removed) = sources.replace(&rel, text)?
        {
            changes.push(Change {
                file: rel,
                what: format!("left out the {api} API"),
                bytes_removed: removed,
            });
        }
    }
    let left_out = apis
        .keys()
        .filter(|a| !kept.contains(*a))
        .cloned()
        .collect();
    Ok(Out {
        changes,
        notes,
        frozen: Frozen::Apis {
            setting: SETTING.to_owned(),
            left_out,
            providers: providers
                .into_iter()
                .map(|(p, a)| (p, a.into_iter().collect()))
                .collect(),
        },
    })
}

fn names<'a>(names: impl Iterator<Item = &'a String>) -> String {
    names.map(String::as_str).collect::<Vec<_>>().join(", ")
}

#[derive(Debug)]
struct Lazy {
    /// The function compat calls, exported by `file`.
    factory: String,
    /// Relative to pi-ai's package directory.
    file: String,
}

/// compat's `const BUILTIN_APIS = [["<api>", <factory>()], …]`, each factory imported from
/// `./api/<name>.lazy.js`.
fn registry(text: &str, rel: &str) -> Result<BTreeMap<String, Lazy>> {
    let allocator = Allocator::default();
    let program = parse(&allocator, text, SourceType::mjs(), rel)?;
    let mut imports = BTreeMap::new();
    let mut list = None;
    for statement in &program.body {
        match statement {
            Statement::ImportDeclaration(d)
                if d.source.value.starts_with("./api/") && d.source.value.ends_with(".lazy.js") =>
            {
                for s in d.specifiers.iter().flatten() {
                    if let ImportDeclarationSpecifier::ImportSpecifier(s) = s {
                        imports.insert(
                            s.local.name.to_string(),
                            format!("dist/{}", &d.source.value[2..]),
                        );
                    }
                }
            }
            Statement::VariableDeclaration(d) => {
                for decl in &d.declarations {
                    if decl.id.get_identifier_name().is_some_and(|n| n == REGISTRY) {
                        match &decl.init {
                            Some(Expression::ArrayExpression(a)) => list = Some(a),
                            _ => bail!("{rel}: `{REGISTRY}` is no longer an array literal"),
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let list = list.ok_or_else(|| anyhow!("{rel}: `const {REGISTRY} = [ … ]` not found"))?;
    let mut apis = BTreeMap::new();
    for element in &list.elements {
        let unexpected = || {
            anyhow!(
                "{rel}: expected `[\"<api>\", <factory>()]` in `{REGISTRY}`; found `{}`",
                snippet(text, element.span())
            )
        };
        let ArrayExpressionElement::ArrayExpression(pair) = element else {
            return Err(unexpected());
        };
        let [
            ArrayExpressionElement::StringLiteral(api),
            ArrayExpressionElement::CallExpression(call),
        ] = &pair.elements[..]
        else {
            return Err(unexpected());
        };
        let Expression::Identifier(factory) = &call.callee else {
            return Err(unexpected());
        };
        if !call.arguments.is_empty() {
            return Err(unexpected());
        }
        let file = imports.get(factory.name.as_str()).ok_or_else(|| {
            anyhow!(
                "{rel}: `{REGISTRY}` calls `{}`, which is not imported from ./api/<name>.lazy.js",
                factory.name
            )
        })?;
        apis.insert(
            api.value.to_string(),
            Lazy {
                factory: factory.name.to_string(),
                file: file.clone(),
            },
        );
    }
    if apis.is_empty() {
        bail!("{rel}: `{REGISTRY}` is empty");
    }
    Ok(apis)
}

/// The new text of `<api>.lazy.js`, or `None` when it already is as wanted. Expects
/// `export const <factory> = () => lazyApi(() => import("./<file>.js"));`.
fn stub_lazy(
    text: &str,
    rel: &str,
    api: &str,
    factory: &str,
    keep: bool,
) -> Result<Option<String>> {
    let allocator = Allocator::default();
    let program = parse(&allocator, text, SourceType::mjs(), rel)?;
    let shape =
        format!("`export const {factory} = () => {LAZY_API}(() => import(\"./<file>.js\"));`");
    let mut loader = None;
    for statement in &program.body {
        let Statement::ExportDeclaration(e) = statement else {
            continue;
        };
        let Declaration::VariableDeclaration(d) = &e.declaration else {
            continue;
        };
        for decl in &d.declarations {
            let BindingPattern::BindingIdentifier(id) = &decl.id else {
                continue;
            };
            if id.name != factory {
                continue;
            }
            let found = || {
                anyhow!(
                    "{rel}: expected {shape}; found `{}`",
                    snippet(text, statement.span())
                )
            };
            let Some(Expression::ArrowFunctionExpression(outer)) = &decl.init else {
                return Err(found());
            };
            let Some(Expression::CallExpression(call)) = outer.get_expression() else {
                return Err(found());
            };
            let ([Argument::ArrowFunctionExpression(inner)], Expression::Identifier(callee)) =
                (&call.arguments[..], &call.callee)
            else {
                return Err(found());
            };
            if callee.name != LAZY_API || !inner.params.items.is_empty() || inner.r#async {
                return Err(found());
            }
            loader = Some((range(inner.span), inner.get_expression().map(|e| match e {
                Expression::ImportExpression(i) => matches!(&i.source, Expression::StringLiteral(s) if s.value.starts_with("./")),
                _ => false,
            })));
        }
    }
    let Some((span, imports)) = loader else {
        bail!("{rel}: `export const {factory}` not found; expected {shape}");
    };
    let stubbed = text[span.clone()] == stub(api);
    match (keep, imports == Some(true), stubbed) {
        (true, true, _) | (false, _, true) => Ok(None),
        (false, true, _) => Ok(Some(splice(text, &mut [(span, stub(api))])?)),
        (true, _, true) => {
            bail!("{rel}: the {api} API is already stubbed out, but {SETTING} keeps it")
        }
        _ => bail!(
            "{rel}: expected {shape}; found `{}`",
            text[span].lines().next().unwrap_or("")
        ),
    }
}

/// The new text of `bedrock-provider.js`, or `None` when it already is as wanted. Expects
/// `import { stream, streamSimple } from "./api/bedrock-converse-stream.js";` and the export of
/// those two.
fn stub_bedrock(text: &str, rel: &str, keep: bool) -> Result<Option<String>> {
    let allocator = Allocator::default();
    let program = parse(&allocator, text, SourceType::mjs(), rel)?;
    let shape = format!("`import {{ stream, streamSimple }} from \"{BEDROCK_IMPORT}\";`");
    let mut import = None;
    for statement in &program.body {
        if let Statement::ImportDeclaration(d) = statement {
            let names: Vec<&str> = d
                .specifiers
                .iter()
                .flatten()
                .map(|s| match s {
                    ImportDeclarationSpecifier::ImportSpecifier(s)
                        if s.imported.name() == s.local.name =>
                    {
                        s.local.name.as_str()
                    }
                    _ => "",
                })
                .collect();
            if d.source.value != BEDROCK_IMPORT || names != ["stream", "streamSimple"] {
                bail!(
                    "{rel}: expected only {shape}; found `{}`",
                    snippet(text, d.span)
                );
            }
            import = Some(range(d.span));
        }
    }
    let stubbed = text.contains(&bedrock_stub());
    match (keep, import, stubbed) {
        (true, Some(_), _) | (false, None, true) => Ok(None),
        (false, Some(span), false) => Ok(Some(splice(text, &mut [(span, bedrock_stub())])?)),
        (true, None, true) => {
            bail!("{rel}: the {BEDROCK} API is already stubbed out, but {SETTING} keeps it")
        }
        _ => bail!("{rel}: expected {shape}"),
    }
}

/// Built-in provider → the stream APIs its catalog models use. Every object in the catalog
/// files with a string `provider` and `api` is a model.
fn catalog(
    dir: &Path,
    apis: &BTreeMap<String, Lazy>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    fn models(v: &Value, out: &mut Vec<(String, String)>) {
        match v {
            Value::Object(o) => {
                if let (Some(Value::String(p)), Some(Value::String(a))) =
                    (o.get("provider"), o.get("api"))
                {
                    out.push((p.clone(), a.clone()));
                }
                o.values().for_each(|v| models(v, out));
            }
            Value::Array(a) => a.iter().for_each(|v| models(v, out)),
            _ => {}
        }
    }
    let mut providers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading pi-ai's catalog {}", dir.display()))?
    {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let value: Value = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut found = vec![];
        models(&value, &mut found);
        for (provider, api) in found {
            let entry = providers.entry(provider).or_default();
            if apis.contains_key(&api) {
                entry.insert(api);
            }
        }
    }
    if providers.is_empty() {
        bail!(
            "{}: no models with a provider and an api; pi-ai's catalog layout changed",
            dir.display()
        );
    }
    Ok(providers)
}

enum Derived {
    /// Every API stays, for this reason.
    All(String),
    /// API → why it stays.
    Some(BTreeMap<String, BTreeSet<String>>),
}

/// The APIs the setup asks for: its default provider's, those of `enabledModels` with a provider
/// prefix, every models.json provider's, and any a baked extension's source names as a string.
/// What cannot be resolved here keeps everything; a too-small set would fail requests.
fn derive(
    setup: &Setup,
    stage: &Path,
    apis: &BTreeMap<String, Lazy>,
    catalog: &BTreeMap<String, BTreeSet<String>>,
) -> Result<Derived> {
    let settings = read_json(&setup.agent_dir.join("settings.json"))?;
    let models = read_json(&setup.agent_dir.join("models.json"))?;
    let custom = models.get("providers").and_then(Value::as_object);
    let apis_of = |provider: &str| -> Option<BTreeSet<String>> {
        let own = custom.and_then(|c| c.get(provider));
        if own.is_none() && !catalog.contains_key(provider) {
            return None;
        }
        let mut found = catalog.get(provider).cloned().unwrap_or_default();
        if let Some(own) = own {
            let model_apis = own
                .get("models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            for api in std::iter::once(own)
                .chain(model_apis)
                .filter_map(|v| v.get("api")?.as_str())
            {
                found.insert(api.to_owned());
            }
        }
        Some(found)
    };
    let mut kept: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut keep = |found: BTreeSet<String>, why: String| {
        for api in found.into_iter().filter(|a| apis.contains_key(a)) {
            kept.entry(api).or_default().insert(why.clone());
        }
    };

    let Some(default) = settings.get("defaultProvider").and_then(Value::as_str) else {
        return Ok(Derived::All(
            "settings.json names no defaultProvider, so Pi picks one at startup".into(),
        ));
    };
    let Some(found) = apis_of(default) else {
        return Ok(Derived::All(format!(
            "settings.json defaultProvider {default} is neither built in nor in models.json"
        )));
    };
    keep(found, format!("settings.json defaultProvider {default}"));
    for pattern in settings
        .get("enabledModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let pattern = pattern.as_str().unwrap_or_default();
        let found = pattern.split_once('/').and_then(|(p, _)| apis_of(p));
        let Some(found) = found else {
            return Ok(Derived::All(format!(
                "settings.json enabledModels {pattern:?} names no known provider, so it may match any"
            )));
        };
        keep(found, format!("settings.json enabledModels {pattern}"));
    }
    for provider in custom.into_iter().flat_map(|c| c.keys()) {
        if let Some(found) = apis_of(provider) {
            keep(found, format!("models.json provider {provider}"));
        }
    }
    for dir in &setup.extension_dirs {
        let mut found = BTreeSet::new();
        scan(&stage.join(dir), &mut |text| {
            for api in apis.keys() {
                if ['"', '\'', '`']
                    .iter()
                    .any(|q| text.contains(&format!("{q}{api}{q}")))
                {
                    found.insert(api.clone());
                }
            }
        })?;
        keep(found, format!("{dir} names it"));
    }
    Ok(Derived::Some(kept))
}

/// `Null` when the file does not exist.
fn read_json(path: &Path) -> Result<Value> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Calls `found` with each JS/TS source under `dir`, outside `node_modules` and `.git`.
fn scan(dir: &Path, found: &mut dyn FnMut(&str)) -> Result<()> {
    if dir.is_file() {
        found(&String::from_utf8_lossy(&fs::read(dir)?));
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if entry.file_type()?.is_symlink() {
                bail!(
                    "{} is a symlinked directory; pibake does not follow those",
                    path.display()
                );
            }
            if name != "node_modules" && name != ".git" {
                scan(&path, found)?;
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SOURCE_EXTENSIONS.contains(&e))
        {
            found(&String::from_utf8_lossy(&fs::read(&path)?));
        }
    }
    Ok(())
}
