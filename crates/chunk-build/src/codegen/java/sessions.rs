use std::{collections::BTreeMap, io};

use chunk_contract::{SessionMethodDeclaration, SessionMethods};
use serde_json::json;

use super::{container, names, quote, support::SupportClass, validate_literals};

type Sessions<'a> = BTreeMap<&'a str, BTreeMap<&'a str, Vec<&'a SessionMethodDeclaration>>>;

pub(super) fn sources(methods: Option<&SessionMethods>, package: &str) -> io::Result<BTreeMap<String, String>> {
    let mut groups = Sessions::new();
    for method in methods.into_iter().flat_map(|value| &value.methods) {
        groups.entry(&method.app).or_default().entry(&method.session).or_default().push(method);
    }
    let mut class = SupportClass::new("SessionMethods")?;
    let mut metadata = Vec::new();
    for (app, sessions) in groups {
        let mut app_scope = class.app(app)?;
        for (session, mut declarations) in sessions {
            let session_name = names::type_name(session, &app_scope.path);
            let mut scope = app_scope.child(&session_name, session)?;
            declarations.sort_by_key(|method| &method.name);
            for method in declarations {
                validate_literals(&method.arguments)?;
                validate_literals(&method.result)?;
                let name = names::type_name(&method.name, &scope.path);
                let mut signature = scope.child(&name, &method.name)?;
                signature.declare("REF", "method reference")?;
                let arguments = class.generator.schema(&mut signature, &method.arguments, "Args", &method.name)?;
                let result = class.generator.schema(&mut signature, &method.result, "Result", &method.name)?;
                let function_name = names::member_name(&method.name);
                signature.declare(&function_name, "implementation method")?;
                scope.declarations.push(format!(
                    "public interface {name} {{\n{} {}({} args);\nSessionMethodRef<{}, {}> REF = new SessionMethodRef<>({}, {}, {}, {}, {});\n{}\n}}",
                    result.ty, function_name, arguments.ty,
                    arguments.ty, result.ty, quote(app), quote(session), quote(&method.name),
                    arguments.json_type(), result.json_type(), signature.declarations.join("\n")
                ));
                metadata.push(json!({
                    "app": app, "session": session, "name": method.name,
                    "interface": format!("{package}.{}", signature.path.join(".")),
                    "binary_interface": format!("{package}.{}", signature.path.join("$")),
                    "function": function_name,
                    "arguments": method.arguments, "result": method.result,
                }));
            }
            app_scope.declarations.push(container(&session_name, &scope.declarations.join("\n")));
        }
        class.finish_app(&app_scope);
    }
    Ok(BTreeMap::from([
        class.source(package)?,
        ("session-methods.json".into(), serde_json::to_string(&json!({"version": 1, "methods": metadata}))?),
    ]))
}
