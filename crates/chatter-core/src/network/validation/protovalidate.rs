//! Declared validation of end-to-end content.
//!
//! The server enforces the `buf.validate` rules on every RPC request it
//! receives, but it never sees decrypted content, so the rules declared on
//! `pkg/proto/chat/v1/content/content.proto` are enforced here instead: on
//! every inbound payload right after decryption, before any handler sees it,
//! and on every outgoing one before encryption.
//!
//! The rules are read from the descriptors the client is generated from
//! (`content_v1::FILE_DESCRIPTOR_SET`, plus `buf.validate.binpb` for the rule
//! schema itself; both written by `make proto-gen`), compiled once into a plan
//! per message, and evaluated over a [`DynamicMessage`] decoded from the same
//! bytes. The semantics follow protovalidate (field presence, `ignore`,
//! `required`, recursion into nested and repeated messages); the Go conformance
//! test in the server repository (`pkg/proto/chat/v1/content`) runs the
//! official validator over the vectors `conformance_vectors_match_go` replays.
//!
//! Only the standard rule kinds `content.proto` declares are understood. A
//! declared rule outside that set (a CEL expression, `string.pattern`, ...) is a
//! compile error: every validation then fails closed, and
//! `every_declared_rule_is_supported` fails the build's tests.

use std::collections::HashMap;
use std::sync::LazyLock;

use prost_reflect::{
    DescriptorPool, DynamicMessage, EnumDescriptor, ExtensionDescriptor, FieldDescriptor, Kind,
    MessageDescriptor, OneofDescriptor, ReflectMessage, Value,
};
use thiserror::Error;

use crate::proto::content_v1;

/// The `buf.validate` schema (validate.proto and the well-known types it
/// imports), built from the protovalidate module pinned in `buf.lock`.
const BUF_VALIDATE_DESCRIPTOR_SET: &[u8] = include_bytes!("../../proto/buf.validate.binpb");

/// Package whose messages carry the declared E2E rules.
const CONTENT_PACKAGE: &str = "chat.v1.content";

/// Why a payload was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContentValidationError {
    /// The bytes are not the claimed message at all.
    #[error("content does not decode: {0}")]
    Decode(String),
    /// A declared rule failed. `field` is a schema path and `rule` a
    /// protovalidate rule id: neither carries content or identities.
    #[error("content violates {rule} at {field}")]
    Violation { field: String, rule: &'static str },
    /// The rules could not be compiled, so nothing is accepted.
    #[error("content rules unavailable: {0}")]
    Unavailable(String),
}

impl ContentValidationError {
    /// Short fixed category for logs.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Decode(_) => "undecodable",
            Self::Violation { .. } => "rule_violation",
            Self::Unavailable(_) => "rules_unavailable",
        }
    }
}

/// A generated content message whose declared rules can be checked.
pub trait ValidatedContent: prost::Message + Default {
    /// Fully qualified protobuf name.
    const FULL_NAME: &'static str;
}

macro_rules! validated_content {
    ($($ty:ident),* $(,)?) => {
        $(impl ValidatedContent for content_v1::$ty {
            const FULL_NAME: &'static str = concat!("chat.v1.content.", stringify!($ty));
        })*
    };
}

// Only the types an inbound path decodes at the top level; nested messages
// are checked through their parent.
validated_content!(Content, DataMessage, BootstrapResponseFrame);

/// Decode inbound E2E bytes and enforce the declared rules before anything
/// else looks at them.
pub fn decode_validated<M: ValidatedContent>(bytes: &[u8]) -> Result<M, ContentValidationError> {
    let message = M::decode(bytes).map_err(|e| ContentValidationError::Decode(e.to_string()))?;
    validate_encoded(M::FULL_NAME, bytes)?;
    Ok(message)
}

/// The one entry point for decrypted inbound E2E bytes: decode, enforce the
/// declared rules, and on refusal log only which path refused and the fixed
/// category (never content, identities, or the offending value). Each caller
/// treats an error exactly as it treats plaintext that does not decode.
pub fn decode_inbound<M: ValidatedContent>(
    bytes: &[u8],
    path: &'static str,
) -> Result<M, ContentValidationError> {
    decode_validated(bytes).inspect_err(|e| {
        let rule = match e {
            ContentValidationError::Violation { rule, .. } => rule,
            _ => "",
        };
        tracing::warn!(
            path,
            message = M::FULL_NAME,
            category = e.category(),
            rule,
            "dropping inbound E2E content"
        );
    })
}

/// [`decode_inbound`] for the `Content` envelope every E2E path carries.
pub fn decode_inbound_content(
    bytes: &[u8],
    path: &'static str,
) -> Result<content_v1::Content, ContentValidationError> {
    decode_inbound(bytes, path)
}

/// Check encoded bytes of `message_name` against its declared rules.
pub fn validate_encoded(message_name: &str, bytes: &[u8]) -> Result<(), ContentValidationError> {
    let rules = RULES
        .as_ref()
        .map_err(|e| ContentValidationError::Unavailable(e.clone()))?;
    let desc = rules
        .pool
        .get_message_by_name(message_name)
        .ok_or_else(|| {
            ContentValidationError::Unavailable(format!("unknown message {message_name}"))
        })?;
    let message = DynamicMessage::decode(desc, bytes)
        .map_err(|e| ContentValidationError::Decode(e.to_string()))?;
    let mut path = Vec::new();
    rules.check_message(&message, &mut path)
}

/// A valid encoded `Content` (a DataMessage carrying `body`) for tests that
/// only need some plaintext to encrypt: the Signal actor refuses anything that
/// is not valid Content (`services::signal::actor::admit_outgoing_content`).
#[cfg(any(test, feature = "testing"))]
pub fn test_plaintext(body: &str) -> Vec<u8> {
    use prost::Message as _;
    content_v1::Content {
        content: Some(content_v1::content::Content::DataMessage(
            content_v1::DataMessage {
                body: body.to_string(),
                timestamp: 1,
                ..Default::default()
            },
        )),
        timestamp: 1,
        ..Default::default()
    }
    .encode_to_vec()
}

// ---------------------------------------------------------------------------
// Compiled rules
// ---------------------------------------------------------------------------

/// One check on a single (non-list) value. Unsigned bounds carry the
/// protovalidate rule id they report (`uint32.gte`, `fixed64.lte`, ...).
#[derive(Debug, Clone)]
enum Check {
    StringMinLen(u64),
    StringMaxLen(u64),
    StringMaxBytes(u64),
    StringUuid,
    BytesLen(u64),
    BytesMaxLen(u64),
    UintGt(u64, &'static str),
    UintGte(u64, &'static str),
    UintLte(u64, &'static str),
    IntGte(i64),
    IntLte(i64),
    EnumDefinedOnly(EnumDescriptor),
}

/// Rules on one value: a singular field, or each element of a list.
#[derive(Debug, Clone, Default)]
struct ValueRules {
    ignore_if_zero: bool,
    checks: Vec<Check>,
}

#[derive(Debug, Clone, Default)]
struct ListRules {
    max_items: Option<u64>,
    items: ValueRules,
}

#[derive(Debug, Clone)]
struct FieldPlan {
    field: FieldDescriptor,
    required: bool,
    value: ValueRules,
    list: Option<ListRules>,
}

#[derive(Debug, Clone, Default)]
struct MessagePlan {
    fields: Vec<FieldPlan>,
    required_oneofs: Vec<OneofDescriptor>,
}

struct CompiledRules {
    pool: DescriptorPool,
    plans: HashMap<String, MessagePlan>,
    /// Every declared rule as `(message.field, rule id)`, for the coverage test.
    #[cfg_attr(not(test), allow(dead_code))]
    declared: Vec<(String, String)>,
}

static RULES: LazyLock<Result<CompiledRules, String>> = LazyLock::new(compile);

fn compile() -> Result<CompiledRules, String> {
    let mut pool = DescriptorPool::decode(BUF_VALIDATE_DESCRIPTOR_SET)
        .map_err(|e| format!("buf.validate descriptors: {e}"))?;
    pool.decode_file_descriptor_set(content_v1::FILE_DESCRIPTOR_SET)
        .map_err(|e| format!("content descriptors: {e}"))?;
    let ext = |name: &str| {
        pool.get_extension_by_name(name)
            .ok_or_else(|| format!("missing extension {name}"))
    };
    let field_ext = ext("buf.validate.field")?;
    let oneof_ext = ext("buf.validate.oneof")?;
    let message_ext = ext("buf.validate.message")?;

    let mut compiler = Compiler {
        unsupported: Vec::new(),
        declared: Vec::new(),
    };
    let mut plans = HashMap::new();
    for desc in pool.all_messages() {
        if desc.package_name() != CONTENT_PACKAGE {
            continue;
        }
        let plan = compiler.message(&desc, &field_ext, &oneof_ext, &message_ext);
        plans.insert(desc.full_name().to_string(), plan);
    }
    if !compiler.unsupported.is_empty() {
        return Err(format!(
            "unsupported rules declared: {}",
            compiler.unsupported.join(", ")
        ));
    }
    Ok(CompiledRules {
        pool,
        plans,
        declared: compiler.declared,
    })
}

struct Compiler {
    unsupported: Vec<String>,
    declared: Vec<(String, String)>,
}

impl Compiler {
    fn message(
        &mut self,
        desc: &MessageDescriptor,
        field_ext: &ExtensionDescriptor,
        oneof_ext: &ExtensionDescriptor,
        message_ext: &ExtensionDescriptor,
    ) -> MessagePlan {
        let mut plan = MessagePlan::default();
        let options = desc.options();
        if options.has_extension(message_ext) {
            // No message-level rule (CEL, message oneof) is understood.
            self.unsupported
                .push(format!("{}: buf.validate.message", desc.full_name()));
        }
        for oneof in desc.oneofs() {
            let options = oneof.options();
            if !options.has_extension(oneof_ext) {
                continue;
            }
            let ext = options.get_extension(oneof_ext);
            let Some(rules) = ext.as_message() else {
                continue;
            };
            let at = oneof.full_name().to_string();
            self.reject_unknown(rules, &at);
            for (f, v) in rules.fields() {
                match (f.name(), v.as_bool()) {
                    ("required", Some(true)) => {
                        self.declared.push((at.clone(), "oneof.required".into()));
                        plan.required_oneofs.push(oneof.clone());
                    }
                    ("required", Some(false)) => {}
                    (name, _) => self.unsupported.push(format!("{at}: oneof.{name}")),
                }
            }
        }
        for field in desc.fields() {
            let options = field.options();
            let mut fp = FieldPlan {
                field: field.clone(),
                required: false,
                value: ValueRules::default(),
                list: None,
            };
            if options.has_extension(field_ext) {
                let ext = options.get_extension(field_ext);
                if let Some(rules) = ext.as_message() {
                    let at = field.full_name().to_string();
                    self.field(rules, &field, &at, &mut fp);
                }
            }
            plan.fields.push(fp);
        }
        plan
    }

    fn reject_unknown(&mut self, rules: &DynamicMessage, at: &str) {
        if rules.unknown_fields().next().is_some() || rules.extensions().next().is_some() {
            self.unsupported
                .push(format!("{at}: unknown or extension rule"));
        }
    }

    fn field(
        &mut self,
        rules: &DynamicMessage,
        field: &FieldDescriptor,
        at: &str,
        fp: &mut FieldPlan,
    ) {
        self.reject_unknown(rules, at);
        for (f, v) in rules.fields() {
            match f.name() {
                "required" => {
                    if v.as_bool() == Some(true) {
                        self.declared.push((at.into(), "required".into()));
                        fp.required = true;
                    }
                }
                "ignore" => match v.as_enum_number() {
                    Some(0) => {}
                    Some(1) => {
                        self.declared
                            .push((at.into(), "ignore.IGNORE_IF_ZERO_VALUE".into()));
                        fp.value.ignore_if_zero = true;
                    }
                    _ => self.unsupported.push(format!("{at}: ignore={v:?}")),
                },
                "repeated" if field.is_list() => {
                    let Some(rep) = v.as_message() else { continue };
                    let list = self.repeated(rep, field, at);
                    fp.list = Some(list);
                }
                kind if !field.is_list() && !field.is_map() => {
                    let Some(typed) = v.as_message() else {
                        self.unsupported.push(format!("{at}: {kind}"));
                        continue;
                    };
                    let checks = self.typed(kind, typed, &field.kind(), at);
                    fp.value.checks.extend(checks);
                }
                other => self.unsupported.push(format!("{at}: {other}")),
            }
        }
    }

    fn repeated(&mut self, rules: &DynamicMessage, field: &FieldDescriptor, at: &str) -> ListRules {
        self.reject_unknown(rules, at);
        let mut list = ListRules::default();
        for (f, v) in rules.fields() {
            match f.name() {
                "max_items" => {
                    self.declared.push((at.into(), "repeated.max_items".into()));
                    list.max_items = v.as_u64();
                }
                "items" => {
                    let Some(items) = v.as_message() else {
                        continue;
                    };
                    self.reject_unknown(items, at);
                    for (f, v) in items.fields() {
                        match (f.name(), v.as_message()) {
                            ("ignore", _) if v.as_enum_number() == Some(1) => {
                                self.declared
                                    .push((at.into(), "items.ignore.IGNORE_IF_ZERO_VALUE".into()));
                                list.items.ignore_if_zero = true;
                            }
                            (kind, Some(typed)) => {
                                let checks = self.typed(kind, typed, &field.kind(), at);
                                list.items.checks.extend(checks);
                            }
                            (other, None) => self.unsupported.push(format!("{at}: items.{other}")),
                        }
                    }
                }
                other => self.unsupported.push(format!("{at}: repeated.{other}")),
            }
        }
        list
    }

    /// One `FieldRules.type` member (`string`, `bytes`, ...) on a field of `kind`.
    fn typed(
        &mut self,
        rule_kind: &str,
        rules: &DynamicMessage,
        kind: &Kind,
        at: &str,
    ) -> Vec<Check> {
        self.reject_unknown(rules, at);
        let matches = matches!(
            (rule_kind, kind),
            ("string", Kind::String)
                | ("bytes", Kind::Bytes)
                | ("uint32", Kind::Uint32)
                | ("uint64", Kind::Uint64)
                | ("fixed64", Kind::Fixed64)
                | ("int32", Kind::Int32)
                | ("enum", Kind::Enum(_))
        );
        if !matches {
            self.unsupported
                .push(format!("{at}: {rule_kind} rules on a {kind:?} field"));
            return Vec::new();
        }
        let mut checks = Vec::new();
        for (f, v) in rules.fields() {
            let id = format!("{rule_kind}.{}", f.name());
            let unsigned = v.as_u32().map(u64::from).or_else(|| v.as_u64());
            let signed = v.as_i32().map(i64::from);
            let check = match (id.as_str(), unsigned, signed) {
                ("string.min_len", Some(n), _) => Some(Check::StringMinLen(n)),
                ("string.max_len", Some(n), _) => Some(Check::StringMaxLen(n)),
                ("string.max_bytes", Some(n), _) => Some(Check::StringMaxBytes(n)),
                ("string.uuid", _, _) if v.as_bool() == Some(true) => Some(Check::StringUuid),
                ("bytes.len", Some(n), _) => Some(Check::BytesLen(n)),
                ("bytes.max_len", Some(n), _) => Some(Check::BytesMaxLen(n)),
                ("uint32.gte", Some(n), _) => Some(Check::UintGte(n, "uint32.gte")),
                ("uint32.lte", Some(n), _) => Some(Check::UintLte(n, "uint32.lte")),
                ("uint64.lte", Some(n), _) => Some(Check::UintLte(n, "uint64.lte")),
                ("fixed64.gt", Some(n), _) => Some(Check::UintGt(n, "fixed64.gt")),
                ("fixed64.lte", Some(n), _) => Some(Check::UintLte(n, "fixed64.lte")),
                ("int32.gte", _, Some(n)) => Some(Check::IntGte(n)),
                ("int32.lte", _, Some(n)) => Some(Check::IntLte(n)),
                ("enum.defined_only", _, _) if v.as_bool() == Some(true) => match kind {
                    Kind::Enum(e) => Some(Check::EnumDefinedOnly(e.clone())),
                    _ => None,
                },
                _ => None,
            };
            match check {
                Some(c) => {
                    self.declared.push((at.into(), id));
                    checks.push(c);
                }
                None => self.unsupported.push(format!("{at}: {id}")),
            }
        }
        checks
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

fn violation(path: &[String], rule: &'static str) -> ContentValidationError {
    ContentValidationError::Violation {
        field: path.join("."),
        rule,
    }
}

fn is_uuid(s: &str) -> bool {
    // protovalidate's string.uuid: 8-4-4-4-12 hex digits, either case.
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

impl CompiledRules {
    fn check_message(
        &self,
        message: &DynamicMessage,
        path: &mut Vec<String>,
    ) -> Result<(), ContentValidationError> {
        let desc = message.descriptor();
        let Some(plan) = self.plans.get(desc.full_name()) else {
            return Err(ContentValidationError::Unavailable(format!(
                "no rules compiled for {}",
                desc.full_name()
            )));
        };
        for oneof in &plan.required_oneofs {
            if !oneof.fields().any(|f| message.has_field(&f)) {
                path.push(oneof.name().to_string());
                return Err(violation(path, "required"));
            }
        }
        for fp in &plan.fields {
            path.push(fp.field.name().to_string());
            self.check_field(message, fp, path)?;
            path.pop();
        }
        Ok(())
    }

    fn check_field(
        &self,
        message: &DynamicMessage,
        fp: &FieldPlan,
        path: &mut Vec<String>,
    ) -> Result<(), ContentValidationError> {
        let field = &fp.field;
        let present = message.has_field(field);
        if fp.required && !present {
            return Err(violation(path, "required"));
        }
        if field.is_list() {
            let value = message.get_field(field);
            let items = value.as_list().unwrap_or_default();
            if let Some(list) = &fp.list
                && !(fp.value.ignore_if_zero && items.is_empty())
            {
                if let Some(max) = list.max_items
                    && items.len() as u64 > max
                {
                    return Err(violation(path, "repeated.max_items"));
                }
                for (i, item) in items.iter().enumerate() {
                    path.push(i.to_string());
                    check_value(item, &list.items, &field.kind(), path)?;
                    path.pop();
                }
            }
            if matches!(field.kind(), Kind::Message(_)) {
                for (i, item) in items.iter().enumerate() {
                    if let Some(m) = item.as_message() {
                        path.push(i.to_string());
                        self.check_message(m, path)?;
                        path.pop();
                    }
                }
            }
            return Ok(());
        }
        if field.is_map() {
            // content.proto declares no maps; nested values would still be
            // checked by their own message plans.
            let value = message.get_field(field);
            if let Some(map) = value.as_map() {
                for v in map.values() {
                    if let Some(m) = v.as_message() {
                        self.check_message(m, path)?;
                    }
                }
            }
            return Ok(());
        }
        if field.supports_presence() && !present {
            return Ok(());
        }
        let value = message.get_field(field);
        check_value(&value, &fp.value, &field.kind(), path)?;
        if let Some(m) = value.as_message() {
            self.check_message(m, path)?;
        }
        Ok(())
    }
}

fn check_value(
    value: &Value,
    rules: &ValueRules,
    kind: &Kind,
    path: &[String],
) -> Result<(), ContentValidationError> {
    if rules.checks.is_empty() || (rules.ignore_if_zero && value.is_default(kind)) {
        return Ok(());
    }
    let unsigned = value.as_u32().map(u64::from).or_else(|| value.as_u64());
    let signed = value.as_i32().map(i64::from);
    for check in &rules.checks {
        let failed: Option<&'static str> = match check {
            Check::StringMinLen(n) => value
                .as_str()
                .filter(|s| (s.chars().count() as u64) < *n)
                .map(|_| "string.min_len"),
            Check::StringMaxLen(n) => value
                .as_str()
                .filter(|s| s.chars().count() as u64 > *n)
                .map(|_| "string.max_len"),
            Check::StringMaxBytes(n) => value
                .as_str()
                .filter(|s| s.len() as u64 > *n)
                .map(|_| "string.max_bytes"),
            Check::StringUuid => match value.as_str() {
                Some("") => Some("string.uuid_empty"),
                Some(s) if !is_uuid(s) => Some("string.uuid"),
                _ => None,
            },
            Check::BytesLen(n) => value
                .as_bytes()
                .filter(|b| b.len() as u64 != *n)
                .map(|_| "bytes.len"),
            Check::BytesMaxLen(n) => value
                .as_bytes()
                .filter(|b| b.len() as u64 > *n)
                .map(|_| "bytes.max_len"),
            Check::UintGt(n, id) => unsigned.filter(|v| v <= n).map(|_| *id),
            Check::UintGte(n, id) => unsigned.filter(|v| v < n).map(|_| *id),
            Check::UintLte(n, id) => unsigned.filter(|v| v > n).map(|_| *id),
            Check::IntGte(n) => signed.filter(|v| v < n).map(|_| "int32.gte"),
            Check::IntLte(n) => signed.filter(|v| v > n).map(|_| "int32.lte"),
            Check::EnumDefinedOnly(e) => value
                .as_enum_number()
                .filter(|n| e.get_value(*n).is_none())
                .map(|_| "enum.defined_only"),
        };
        if let Some(rule) = failed {
            return Err(violation(path, rule));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    /// Every rule id the evaluator enforces: exactly the kinds content.proto
    /// declares. Anything else compiles to an error.
    const SUPPORTED_RULES: &[&str] = &[
        "required",
        "ignore.IGNORE_IF_ZERO_VALUE",
        "oneof.required",
        "repeated.max_items",
        "string.min_len",
        "string.max_len",
        "string.max_bytes",
        "string.uuid",
        "bytes.len",
        "bytes.max_len",
        "uint32.gte",
        "uint32.lte",
        "uint64.lte",
        "int32.gte",
        "int32.lte",
        "fixed64.gt",
        "fixed64.lte",
        "enum.defined_only",
    ];

    fn rules() -> &'static CompiledRules {
        RULES.as_ref().expect("content rules compile")
    }

    /// Every rule content.proto declares compiles into a check. A rule kind the
    /// evaluator does not understand would otherwise go silently unenforced.
    #[test]
    fn every_declared_rule_is_supported() {
        let compiled = compile();
        if let Err(e) = &compiled {
            panic!("{e}");
        }
        let declared = &rules().declared;
        assert!(
            declared.len() > 50,
            "content.proto should declare its rules; found {}",
            declared.len()
        );
        let mut kinds: Vec<&str> = declared.iter().map(|(_, id)| id.as_str()).collect();
        kinds.sort_unstable();
        kinds.dedup();
        for kind in &kinds {
            let base = kind.strip_prefix("items.").unwrap_or(kind);
            assert!(
                SUPPORTED_RULES.contains(&base),
                "declared rule {kind} is not supported"
            );
        }
        // Every supported kind is actually declared, so none is dead weight
        // the conformance vectors would not exercise.
        for kind in SUPPORTED_RULES {
            assert!(
                kinds
                    .iter()
                    .any(|k| k.strip_prefix("items.").unwrap_or(k) == *kind),
                "supported rule {kind} is declared nowhere in content.proto"
            );
        }
    }

    #[test]
    fn uuid_matches_protovalidate() {
        assert!(is_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_uuid("550E8400-E29B-41D4-A716-446655440000"));
        assert!(!is_uuid("550e8400e29b41d4a716446655440000"));
        assert!(!is_uuid("550e8400-e29b-41d4-a716-44665544000g"));
        assert!(!is_uuid(""));
    }

    /// The same vectors the Go test validates with buf.build/go/protovalidate
    /// (pkg/proto/chat/v1/content/content_validation_vector_test.go). Every
    /// case must get the same verdict here.
    #[test]
    fn conformance_vectors_match_go() {
        let vectors = include_str!("testdata/content_validation_vectors.tsv");
        let mut cases = 0;
        let mut mismatches = Vec::new();
        for line in vectors.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 4, "malformed vector line: {line}");
            let (name, verdict, message, hex_bytes) = (cols[0], cols[1], cols[2], cols[3]);
            let bytes = hex::decode(hex_bytes).expect("vector hex");
            let want_valid = match verdict {
                "valid" => true,
                "invalid" => false,
                other => panic!("bad verdict {other} in {name}"),
            };
            let got = validate_encoded(message, &bytes);
            if got.is_ok() != want_valid {
                mismatches.push(format!("{name}: go={verdict} rust={got:?}"));
            }
            cases += 1;
        }
        assert!(cases >= 50, "expected at least 50 vectors, got {cases}");
        assert!(
            mismatches.is_empty(),
            "verdicts differ:\n{}",
            mismatches.join("\n")
        );
    }

    #[test]
    fn decode_inbound_rejects_violation_and_accepts_valid() {
        let ok = content_v1::Content {
            content: Some(content_v1::content::Content::NullMessage(
                content_v1::NullMessage::default(),
            )),
            timestamp: 1,
            ..Default::default()
        };
        assert!(decode_inbound_content(&ok.encode_to_vec(), "test").is_ok());
        let empty = content_v1::Content::default();
        let err = decode_inbound_content(&empty.encode_to_vec(), "test").unwrap_err();
        assert_eq!(err.category(), "rule_violation");
        assert!(matches!(
            decode_inbound_content(b"\xff\xff\xff", "test"),
            Err(ContentValidationError::Decode(_))
        ));
    }
}
