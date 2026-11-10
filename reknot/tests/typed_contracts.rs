#![cfg(feature = "typed")]
use reknot::typed::{Error, ExportedModule, Handler, Module};
use reknot::{HandlerApi, ServiceError};
use serde::{Deserialize, Serialize};

#[reknot::schema]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Message {
    user_name: String,
    #[serde(default)]
    attempts: u32,
    #[serde(skip_serializing)]
    internal: Option<String>,
}
const HANDLE: Handler<Message, String> = Handler::new("schema.message", 1);
fn module() -> Module {
    Module::new("schema-test", 1).with(HANDLE)
}
#[reknot::handler(HANDLE)]
fn handle(message: Message) -> String {
    let _ = message.internal;
    message.user_name
}
reknot::export_handlers!(module(), [handle]);

fn metadata(api: HandlerApi) -> ExportedModule {
    let mut bytes = vec![0; reknot::MAX_INIT_BYTES];
    let length = unsafe { api.describe.unwrap()(bytes.as_mut_ptr(), bytes.len()) };
    serde_json::from_slice(&bytes[..length]).unwrap()
}

#[test]
fn serde_shape_is_exported_in_both_directions_with_handler_source_location() {
    let manifest = metadata(handler_api());
    let entry = &manifest.handlers[0];
    assert!(entry.input_schema.deserialize["properties"]
        .get("userName")
        .is_some());
    assert!(entry.input_schema.deserialize["properties"]
        .get("internal")
        .is_some());
    assert!(entry.input_schema.serialize["properties"]
        .get("internal")
        .is_none());
    assert_eq!(
        entry.input_schema.deserialize["additionalProperties"],
        false
    );
    let required = entry.input_schema.deserialize["required"]
        .as_array()
        .unwrap();
    assert!(!required.contains(&serde_json::json!("attempts")));
    let source = entry.source.as_ref().unwrap();
    assert!(source.file.ends_with("typed_contracts.rs"));
    assert!(source.function.ends_with("::handle"));
    assert!(source.line > 0);
    let document: serde_json::Value =
        serde_json::from_str(&module().schema_json().unwrap()).unwrap();
    assert_eq!(document["format"], "reknot-typed-contract-v2");
    assert_eq!(
        document["module"]["handlers"][0]["input_schema"],
        serde_json::to_value(&entry.input_schema).unwrap()
    );
}

#[test]
fn same_rust_type_name_and_version_do_not_hide_a_field_shape_change() {
    // A serialized host manifest remains the old contract after a DLL rebuild.
    let mut old = serde_json::to_value(module()).unwrap();
    old["handlers"][0]["input_schema"]["deserialize"]["properties"]["userName"]["type"] =
        "integer".into();
    let old: Module = serde_json::from_value(old).unwrap();
    assert_eq!(old.handlers()[0].input, module().handlers()[0].input);
    let error = unsafe { old.builder().from_api(handler_api()) }
        .err()
        .unwrap();
    let Error::Contract { report, .. } = &error else {
        panic!("{error}")
    };
    let difference = report
        .differences
        .iter()
        .find(|d| d.path.ends_with("/properties/userName/type"))
        .unwrap();
    assert_eq!(difference.expected, Some("integer".into()));
    assert_eq!(difference.actual, Some("string".into()));
    assert_eq!(difference.handler.as_deref(), Some("schema.message"));
    assert!(difference.source.is_some());
    assert!(error.to_string().contains("typed_contracts.rs"));
    assert!(report.candidate.is_some());
    serde_json::to_string(report).unwrap();
}

#[test]
fn missing_exports_and_versions_are_distinguished_without_using_source_paths_as_contracts() {
    let actual = metadata(handler_api());
    let mut changed = actual.clone();
    changed.handlers.clear();
    let report = reknot::typed::ContractReport::new(&module(), Some(changed));
    assert!(report
        .differences
        .iter()
        .any(|d| d.stage == "declaration-to-export" && d.actual.is_none()));
    let mut changed = actual.clone();
    changed.module.version += 1;
    let report = reknot::typed::ContractReport::new(&module(), Some(changed));
    assert!(report
        .differences
        .iter()
        .any(|d| d.path == "/version" && d.handler.is_none()));
    let mut relocated = actual;
    relocated.handlers[0].source.as_mut().unwrap().file = "different-checkout/src/lib.rs".into();
    assert!(
        reknot::typed::ContractReport::new(&module(), Some(relocated))
            .differences
            .is_empty()
    );
}

#[test]
fn invalid_or_oversized_diagnostics_do_not_replace_the_original_rejection() {
    unsafe extern "C" fn reject(_: *const u8, _: usize) -> u32 {
        ServiceError::CONTRACT_MISMATCH.0
    }
    unsafe extern "C" fn oversized(_: *mut u8, _: usize) -> usize {
        usize::MAX
    }
    let mut api = handler_api();
    api.validate_init = Some(reject);
    api.describe = Some(oversized);
    let error = unsafe { module().builder().from_api(api) }.err().unwrap();
    let Error::Contract { report, source } = error else {
        panic!("expected contract error")
    };
    assert!(report.candidate.is_none());
    assert!(matches!(
        source,
        reknot::RouteError::InitData(ServiceError::CONTRACT_MISMATCH)
    ));
}
