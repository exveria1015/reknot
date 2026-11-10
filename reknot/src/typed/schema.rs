use super::*;
use crate::{Checkpoint, RequestContext};
use schemars::generate::SchemaSettings;

/// Both Serde directions are recorded: skip/default/rename rules may differ.
/// Equality is conservative schema equality, not a proof of semantic compatibility.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeSchema {
    pub serialize: serde_json::Value,
    pub deserialize: serde_json::Value,
}
impl TypeSchema {
    pub fn of<T: JsonSchema>() -> Self {
        Self {
            serialize: SchemaSettings::draft2020_12()
                .for_serialize()
                .into_generator()
                .into_root_schema_for::<T>()
                .to_value(),
            deserialize: SchemaSettings::draft2020_12()
                .for_deserialize()
                .into_generator()
                .into_root_schema_for::<T>()
                .to_value(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub function: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSpec {
    pub name: String,
    pub version: u32,
    pub rust_type: String,
    pub schema: TypeSchema,
}
impl StateSpec {
    pub fn key(&self) -> ServiceKey {
        ServiceKey::new(key(&self.name), self.version)
    }
}

/// The module's versioned, typed worker checkpoint contract.
pub struct WorkerState<S> {
    name: &'static str,
    version: u32,
    marker: PhantomData<fn() -> S>,
}
impl<S> Copy for WorkerState<S> {}
impl<S> Clone for WorkerState<S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> WorkerState<S> {
    pub const fn new(name: &'static str, version: u32) -> Self {
        Self {
            name,
            version,
            marker: PhantomData,
        }
    }
}
#[derive(Serialize, Deserialize)]
struct StateEnvelope<S> {
    contract: StateSpec,
    value: S,
}
impl<S: Serialize + DeserializeOwned + JsonSchema + 'static> WorkerState<S> {
    pub fn descriptor(self) -> StateSpec {
        StateSpec {
            name: self.name.into(),
            version: self.version,
            rust_type: std::any::type_name::<S>().into(),
            schema: TypeSchema::of::<S>(),
        }
    }
    pub fn encode(self, value: &S) -> Result<Checkpoint, Error> {
        if !valid_name(self.name) || self.version == 0 {
            return Err(Error::Configuration(
                "worker state needs a nonempty name and positive version".into(),
            ));
        }
        let contract = self.descriptor();
        Ok(Checkpoint::new(
            contract.key(),
            encode(&StateEnvelope { contract, value }, MAX_MESSAGE_BYTES)?,
        ))
    }
    pub fn decode(self, checkpoint: &Checkpoint) -> Result<S, Error> {
        if checkpoint.bytes.len() > MAX_MESSAGE_BYTES {
            return Err(Error::TooLarge {
                limit: MAX_MESSAGE_BYTES,
            });
        }
        // Validate the contract before invoking application deserializers. Read
        // the original bytes again afterwards to preserve JSON numeric values.
        let envelope: StateEnvelope<serde::de::IgnoredAny> =
            serde_json::from_slice(&checkpoint.bytes)
                .map_err(|e| Error::Codec(format!("worker checkpoint: {e}")))?;
        let expected = self.descriptor();
        if checkpoint.schema != expected.key() || envelope.contract != expected {
            return Err(Error::Configuration(format!("worker checkpoint does not match {} v{} and its structural schema; decode with the old WorkerState and migrate explicitly", self.name, self.version)));
        }
        serde_json::from_slice::<StateEnvelope<S>>(&checkpoint.bytes)
            .map(|envelope| envelope.value)
            .map_err(|e| Error::Codec(format!("worker checkpoint: {e}")))
    }
    pub fn restore(self, context: &RequestContext) -> Result<S, Error> {
        self.decode(
            context.checkpoint().ok_or_else(|| {
                Error::Configuration("this request has no worker checkpoint".into())
            })?,
        )
    }
    pub fn save(self, context: &RequestContext, value: &S) -> Result<(), Error> {
        let checkpoint = self.encode(value)?;
        context
            .save_checkpoint(checkpoint.schema, &checkpoint.bytes)
            .map_err(|error| Error::Route(RouteError::Resource(error)))
    }
}

/// Metadata copied from the candidate; no DLL-owned pointers survive validation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedModule {
    pub format_version: u32,
    pub module: Module,
    pub handlers: Vec<HandlerSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContractDifference {
    /// host-to-candidate or declaration-to-export.
    pub stage: String,
    pub handler: Option<String>,
    /// JSON pointer within this handler/module contract.
    pub path: String,
    pub expected: Option<serde_json::Value>,
    pub actual: Option<serde_json::Value>,
    pub source: Option<SourceLocation>,
}

/// Bounded differences plus the original manifests for inspection/export.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContractReport {
    pub expected: Module,
    pub candidate: Option<ExportedModule>,
    pub differences: Vec<ContractDifference>,
    pub truncated: bool,
}
impl ContractReport {
    pub fn new(expected: &Module, candidate: Option<ExportedModule>) -> Self {
        let mut report = Self {
            expected: expected.clone(),
            candidate: None,
            differences: Vec::new(),
            truncated: false,
        };
        if let Some(actual) = &candidate {
            report.compare_module(expected, &actual.module, &actual.handlers);
            report.compare_handlers(
                "declaration-to-export",
                &actual.module.handlers,
                &actual.handlers,
                &actual.handlers,
            );
        }
        report.candidate = candidate;
        report
    }
    fn compare_module(&mut self, expected: &Module, actual: &Module, locations: &[HandlerSpec]) {
        self.compare_value(
            "host-to-candidate",
            None,
            None,
            "/name",
            Some(&serde_json::json!(expected.name)),
            Some(&serde_json::json!(actual.name)),
        );
        self.compare_value(
            "host-to-candidate",
            None,
            None,
            "/version",
            Some(&serde_json::json!(expected.version)),
            Some(&serde_json::json!(actual.version)),
        );
        self.compare_value(
            "host-to-candidate",
            None,
            None,
            "/state",
            Some(&serde_json::json!(expected.state)),
            Some(&serde_json::json!(actual.state)),
        );
        self.compare_handlers(
            "host-to-candidate",
            &expected.handlers,
            &actual.handlers,
            locations,
        );
    }
    fn compare_handlers(
        &mut self,
        stage: &str,
        expected: &[HandlerSpec],
        actual: &[HandlerSpec],
        locations: &[HandlerSpec],
    ) {
        let names: std::collections::BTreeSet<_> = expected
            .iter()
            .chain(actual)
            .map(|h| h.name.as_str())
            .collect();
        for name in names {
            let a = expected.iter().find(|h| h.name == name);
            let b = actual.iter().find(|h| h.name == name);
            let source = locations
                .iter()
                .find(|h| h.name == name)
                .and_then(|h| h.source.as_ref());
            // Source locations are diagnostic metadata, not wire compatibility.
            let value = |handler: &HandlerSpec| {
                let mut value =
                    serde_json::to_value(handler).expect("serializable handler metadata");
                value.as_object_mut().unwrap().remove("source");
                value
            };
            self.compare_value(
                stage,
                Some(name),
                source,
                "",
                a.map(value).as_ref(),
                b.map(value).as_ref(),
            );
        }
    }
    fn compare_value(
        &mut self,
        stage: &str,
        handler: Option<&str>,
        source: Option<&SourceLocation>,
        path: &str,
        expected: Option<&serde_json::Value>,
        actual: Option<&serde_json::Value>,
    ) {
        if expected == actual {
            return;
        }
        if self.differences.len() >= 32 {
            self.truncated = true;
            return;
        }
        if let (Some(serde_json::Value::Object(a)), Some(serde_json::Value::Object(b))) =
            (expected, actual)
        {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                self.compare_value(
                    stage,
                    handler,
                    source,
                    &format!("{path}/{escaped}"),
                    a.get(key),
                    b.get(key),
                );
            }
        } else {
            self.differences.push(ContractDifference {
                stage: stage.into(),
                handler: handler.map(str::to_owned),
                path: path.into(),
                expected: expected.cloned(),
                actual: actual.cloned(),
                source: source.cloned(),
            });
        }
    }
}
impl fmt::Display for ContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "module {} rejected", self.expected.name)?;
        let short = |value: &Option<serde_json::Value>| -> String {
            let text = value
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "<missing>".into());
            if text.chars().count() > 240 {
                format!("{}…", text.chars().take(240).collect::<String>())
            } else {
                text
            }
        };
        for difference in &self.differences {
            write!(
                f,
                "\n  {} {}{}: expected {}, actual {}",
                difference.stage,
                difference.handler.as_deref().unwrap_or("module"),
                difference.path,
                short(&difference.expected),
                short(&difference.actual)
            )?;
            if let Some(source) = &difference.source {
                write!(
                    f,
                    " ({} at {}:{}:{})",
                    source.function, source.file, source.line, source.column
                )?;
            }
        }
        if self.differences.is_empty() {
            write!(f, "; expected handlers: ")?;
            for handler in &self.expected.handlers {
                write!(f, "{} v{}; ", handler.name, handler.version)?;
            }
            write!(f, "the validator rejected the candidate without a structural difference; inspect the underlying error")?;
        }
        if self.truncated {
            write!(
                f,
                "\n  further differences omitted; full manifests are available in the report"
            )?;
        }
        write!(f, "\nCheck shared protocol and exported handlers. Rebuild/restart the host for handler contract changes; use explicit worker-state migration for checkpoint changes.")
    }
}
