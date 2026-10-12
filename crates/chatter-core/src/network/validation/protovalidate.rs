//! Declared validation of end-to-end content and RPC responses.
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
//! Content and RPC outputs share the evaluator. RPC plans include only messages
//! reachable from service outputs, so request-only rules cannot break replies.
//! The finite response CEL expressions and patterns are matched exactly; a new
//! declaration fails closed and must gain support plus Go conformance vectors.
//!
//! Only explicitly supported declared rule kinds are understood. A
//! declared rule outside that set is a
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
    StringLen(u64),
    StringPattern(String),
    BytesMinLen(u64),
    EnumNotIn(Vec<i32>),
    DurationBound(i128, bool),
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
    min_items: Option<u64>,
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
    expressions: Vec<String>,
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

static RPC_RULES: LazyLock<Result<CompiledRules, String>> = LazyLock::new(compile_rpc);

fn compile_rpc() -> Result<CompiledRules, String> {
    let mut pool = DescriptorPool::global();
    pool.decode_file_descriptor_set(BUF_VALIDATE_DESCRIPTOR_SET)
        .map_err(|e| e.to_string())?;
    for bytes in [
        crate::proto::content_v1::FILE_DESCRIPTOR_SET,
        crate::proto::signal_v1::FILE_DESCRIPTOR_SET,
        crate::proto::chat_v1::FILE_DESCRIPTOR_SET,
        crate::proto::mailbox_v1::FILE_DESCRIPTOR_SET,
        crate::proto::provisioning_v1::FILE_DESCRIPTOR_SET,
    ] {
        pool.decode_file_descriptor_set(bytes)
            .map_err(|e| e.to_string())?;
    }
    let mut pending: Vec<_> = pool
        .services()
        .flat_map(|s| s.methods().map(|m| m.output()).collect::<Vec<_>>())
        .collect();
    let mut compiler = Compiler {
        unsupported: Vec::new(),
        declared: Vec::new(),
    };
    let mut plans = HashMap::new();
    while let Some(desc) = pending.pop() {
        if plans.contains_key(desc.full_name()) {
            continue;
        }
        for field in desc.fields() {
            if let Kind::Message(nested) = field.kind() {
                pending.push(nested);
            }
        }
        let plan = compiler.message(
            &desc,
            &pool.get_extension_by_name("buf.validate.field").unwrap(),
            &pool.get_extension_by_name("buf.validate.oneof").unwrap(),
            &pool.get_extension_by_name("buf.validate.message").unwrap(),
        );
        plans.insert(desc.full_name().to_string(), plan);
    }
    if !compiler.unsupported.is_empty() {
        return Err(compiler.unsupported.join(", "));
    }
    Ok(CompiledRules {
        pool,
        plans,
        declared: compiler.declared,
    })
}

/// Validate the exact wire bytes against the output descriptor of the RPC.
pub fn validate_rpc_response(path: &str, bytes: &[u8]) -> Result<(), ContentValidationError> {
    let rules = RPC_RULES
        .as_ref()
        .map_err(|e| ContentValidationError::Unavailable(e.clone()))?;
    let (service, method) = path
        .strip_prefix('/')
        .and_then(|s| s.split_once('/'))
        .ok_or_else(|| ContentValidationError::Unavailable("unknown RPC".into()))?;
    let output = rules
        .pool
        .get_service_by_name(service)
        .and_then(|s| s.methods().find(|m| m.name() == method))
        .ok_or_else(|| ContentValidationError::Unavailable("unknown RPC".into()))?
        .output();
    let message = DynamicMessage::decode(output, bytes)
        .map_err(|_| ContentValidationError::Decode("invalid protobuf".into()))?;
    rules.check_message(
        &message,
        &mut vec![message.descriptor().full_name().to_owned()],
    )
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
            let extension = options.get_extension(message_ext);
            if let Some(rules) = extension.as_message() {
                self.reject_unknown(rules, desc.full_name());
                for (field, value) in rules.fields() {
                    if field.name() != "cel" {
                        self.unsupported.push(format!(
                            "{}: message.{}",
                            desc.full_name(),
                            field.name()
                        ));
                        continue;
                    }
                    for rule in value.as_list().into_iter().flatten() {
                        let expression = rule
                            .as_message()
                            .and_then(|r| r.get_field_by_name("expression"))
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default();
                        let expected_message = match expression.as_str() {
                            "!this.ephemeral || has(this.ephemeral_ttl)" => "chat.v1.Message",
                            "this.type != 4 || this.members.size() == 1"
                            | "this.type == 4 || this.name != ''" => "chat.v1.Channel",
                            "this.persistence != 1 || has(this.ttl)" => "chat.v1.Channel.Config",
                            "this.user_id != this.moderator_id"
                            | "!has(this.duration) || this.action == 2 || this.action == 3"
                            | "this.action == 5 || this.moderator_id != ''" => {
                                "chat.v1.ChannelModeration"
                            }
                            "this.inviter_id != this.invitee_id" => "chat.v1.ChannelInvite",
                            "int(this.expires_at) > int(now) * 1000000000" => "chat.v1.Session",
                            _ => "",
                        };
                        if SUPPORTED_EXPRESSIONS.contains(&expression.as_str())
                            && desc.full_name() == expected_message
                        {
                            plan.expressions.push(expression);
                        } else {
                            self.unsupported
                                .push(format!("{}: unsupported CEL expression", desc.full_name()));
                        }
                    }
                }
            }
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
                "min_items" => {
                    self.declared.push((at.into(), "repeated.min_items".into()));
                    list.min_items = v.as_u64();
                }
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
                | ("duration", Kind::Message(_))
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
                ("string.len", Some(n), _) => Some(Check::StringLen(n)),
                ("string.pattern", _, _) => v
                    .as_str()
                    .filter(|p| SUPPORTED_PATTERNS.contains(p))
                    .map(|p| Check::StringPattern(p.into())),
                ("bytes.min_len", Some(n), _) => Some(Check::BytesMinLen(n)),
                ("enum.not_in", _, _) => v.as_list().map(|items| {
                    Check::EnumNotIn(items.iter().filter_map(Value::as_i32).collect())
                }),
                ("duration.gt", _, _) | ("duration.lt", _, _) => v
                    .as_message()
                    .map(|m| Check::DurationBound(duration_nanos(m), id == "duration.gt")),
                ("uint32.gt", Some(n), _) => Some(Check::UintGt(n, "uint32.gt")),
                ("uint64.gt", Some(n), _) => Some(Check::UintGt(n, "uint64.gt")),
                ("fixed64.gte", Some(n), _) => Some(Check::UintGte(n, "fixed64.gte")),
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

// These are the complete expressions and patterns reachable from RPC outputs.
// Match the exact declared text: additions/changes fail closed, never get ignored.
const SUPPORTED_PATTERNS: &[&str] = &[
    "^[a-zA-Z0-9_-]+$",
    "^[^<>]*$",
    "^https?://.+",
    "^$|^chatter_[0-9a-f]{32}$",
];
fn check_pattern(pattern: &str, s: &str) -> bool {
    match pattern {
        "^[a-zA-Z0-9_-]+$" => {
            !s.is_empty()
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        }
        "^[^<>]*$" => !s.contains(['<', '>']),
        "^https?://.+" => s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .is_some_and(|tail| tail.chars().next().is_some_and(|c| c != '\n')),
        "^$|^chatter_[0-9a-f]{32}$" => {
            s.is_empty()
                || s.strip_prefix("chatter_").is_some_and(|tail| {
                    tail.len() == 32
                        && tail
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                })
        }
        _ => false,
    }
}
const SUPPORTED_EXPRESSIONS: &[&str] = &[
    "!this.ephemeral || has(this.ephemeral_ttl)",
    "this.type != 4 || this.members.size() == 1",
    "this.type == 4 || this.name != ''",
    "this.persistence != 1 || has(this.ttl)",
    "this.user_id != this.moderator_id",
    "!has(this.duration) || this.action == 2 || this.action == 3",
    "this.inviter_id != this.invitee_id",
    "int(this.expires_at) > int(now) * 1000000000",
    "this.action == 5 || this.moderator_id != ''",
];
fn expression_rule_id(expression: &str) -> &'static str {
    match expression {
        "!this.ephemeral || has(this.ephemeral_ttl)" => "ephemeral_ttl_required",
        "this.type != 4 || this.members.size() == 1" => "saved_messages_channel_members",
        "this.type == 4 || this.name != ''" => "channel_name_required",
        "this.persistence != 1 || has(this.ttl)" => "ttl_required_for_ephemeral",
        "this.user_id != this.moderator_id" => "different_moderator_user",
        "!has(this.duration) || this.action == 2 || this.action == 3" => "duration_for_ban_or_mute",
        "this.inviter_id != this.invitee_id" => "different_users",
        "int(this.expires_at) > int(now) * 1000000000" => "session_expiry_in_future",
        "this.action == 5 || this.moderator_id != ''" => "moderator_required_unless_unmute",
        _ => "message.cel",
    }
}
fn duration_nanos(m: &DynamicMessage) -> i128 {
    let seconds = m
        .get_field_by_name("seconds")
        .and_then(|v| v.as_i64())
        .unwrap_or_default();
    let nanos = m
        .get_field_by_name("nanos")
        .and_then(|v| v.as_i32())
        .unwrap_or_default();
    i128::from(seconds) * 1_000_000_000 + i128::from(nanos)
}
fn check_expression(m: &DynamicMessage, expression: &str) -> bool {
    let field = |name: &str| m.get_field_by_name(name).unwrap().into_owned();
    let has = |name: &str| m.has_field_by_name(name);
    match expression {
        "!this.ephemeral || has(this.ephemeral_ttl)" => {
            field("ephemeral").as_bool() == Some(false) || has("ephemeral_ttl")
        }
        "this.type != 4 || this.members.size() == 1" => {
            field("type").as_enum_number() != Some(4)
                || field("members").as_list().is_some_and(|l| l.len() == 1)
        }
        "this.type == 4 || this.name != ''" => {
            field("type").as_enum_number() == Some(4) || field("name").as_str() != Some("")
        }
        "this.persistence != 1 || has(this.ttl)" => {
            field("persistence").as_enum_number() != Some(1) || has("ttl")
        }
        "this.user_id != this.moderator_id" => field("user_id") != field("moderator_id"),
        "!has(this.duration) || this.action == 2 || this.action == 3" => {
            !has("duration") || matches!(field("action").as_enum_number(), Some(2 | 3))
        }
        "this.action == 5 || this.moderator_id != ''" => {
            field("action").as_enum_number() == Some(5)
                || field("moderator_id").as_str() != Some("")
        }
        "this.inviter_id != this.invitee_id" => field("inviter_id") != field("invitee_id"),
        "int(this.expires_at) > int(now) * 1000000000" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            field("expires_at").as_u64().is_some_and(|expiry| {
                expiry <= i64::MAX as u64 && expiry > now.saturating_mul(1_000_000_000)
            })
        }
        _ => false,
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
        for expression in &plan.expressions {
            if !check_expression(message, expression) {
                return Err(violation(path, expression_rule_id(expression)));
            }
        }
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
                if let Some(min) = list.min_items
                    && (items.len() as u64) < min
                {
                    return Err(violation(path, "repeated.min_items"));
                }
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
            Check::StringLen(n) => value
                .as_str()
                .filter(|s| s.chars().count() as u64 != *n)
                .map(|_| "string.len"),
            Check::StringPattern(pattern) => value
                .as_str()
                .filter(|s| !check_pattern(pattern, s))
                .map(|_| "string.pattern"),
            Check::BytesMinLen(n) => value
                .as_bytes()
                .filter(|b| (b.len() as u64) < *n)
                .map(|_| "bytes.min_len"),
            Check::EnumNotIn(items) => value
                .as_enum_number()
                .filter(|n| items.contains(n))
                .map(|_| "enum.not_in"),
            Check::DurationBound(bound, greater) => value
                .as_message()
                .filter(|m| {
                    if *greater {
                        duration_nanos(m) <= *bound
                    } else {
                        duration_nanos(m) >= *bound
                    }
                })
                .map(|_| {
                    if *greater {
                        "duration.gt"
                    } else {
                        "duration.lt"
                    }
                }),
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

    #[test]
    fn rpc_conformance_vectors_match_official_go_validator() {
        let rules = RPC_RULES.as_ref().unwrap();
        let mut count = 0;
        for line in include_str!("testdata/rpc_validation_vectors.tsv").lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let columns: Vec<_> = line.split('\t').collect();
            let bytes = hex::decode(columns[3]).unwrap();
            let message = DynamicMessage::decode(
                rules.pool.get_message_by_name(columns[0]).unwrap(),
                bytes.as_slice(),
            )
            .unwrap();
            let result = rules.check_message(&message, &mut Vec::new());
            assert_eq!(
                result.is_ok(),
                columns[2] == "true",
                "{}: {:?}",
                columns[1],
                result
            );
            count += 1;
        }
        assert!(count >= 60);
    }

    #[test]
    fn rpc_additional_scalar_rules_reject_each_violation_class() {
        let cases = [
            (
                Check::StringLen(3),
                Value::String("ab".into()),
                Kind::String,
                "string.len",
            ),
            (
                Check::StringPattern("^[a-zA-Z0-9_-]+$".into()),
                Value::String("private value".into()),
                Kind::String,
                "string.pattern",
            ),
            (
                Check::BytesMinLen(2),
                Value::Bytes(vec![1].into()),
                Kind::Bytes,
                "bytes.min_len",
            ),
            (
                Check::EnumNotIn(vec![0]),
                Value::EnumNumber(0),
                Kind::Enum(
                    RPC_RULES
                        .as_ref()
                        .unwrap()
                        .pool
                        .get_enum_by_name("chat.v1.ChannelRole")
                        .unwrap(),
                ),
                "enum.not_in",
            ),
            (
                Check::UintGt(0, "uint32.gt"),
                Value::U32(0),
                Kind::Uint32,
                "uint32.gt",
            ),
            (
                Check::UintGt(0, "uint64.gt"),
                Value::U64(0),
                Kind::Uint64,
                "uint64.gt",
            ),
            (
                Check::UintGte(1, "fixed64.gte"),
                Value::U64(0),
                Kind::Fixed64,
                "fixed64.gte",
            ),
        ];
        for (check, value, kind, expected) in cases {
            let error = check_value(
                &value,
                &ValueRules {
                    checks: vec![check],
                    ..Default::default()
                },
                &kind,
                &["schema_field".into()],
            )
            .unwrap_err();
            assert!(
                matches!(error, ContentValidationError::Violation { rule, .. } if rule == expected)
            );
        }
    }

    #[test]
    fn rpc_patterns_match_declared_semantics() {
        for (pattern, valid, invalid) in [
            ("^[a-zA-Z0-9_-]+$", "Test_123-a", "é"),
            ("^[^<>]*$", "hello\nworld", "<hello>"),
            (
                "^https?://.+",
                "https://example.test/",
                "https://\nexample.test",
            ),
            (
                "^$|^chatter_[0-9a-f]{32}$",
                "chatter_0123456789abcdef0123456789abcdef",
                "chatter_0123456789ABCDEF0123456789ABCDEF",
            ),
        ] {
            assert!(check_pattern(pattern, valid));
            assert!(!check_pattern(pattern, invalid));
        }
        assert!(check_pattern("^$|^chatter_[0-9a-f]{32}$", ""));
    }

    #[test]
    fn rpc_cel_expressions_accept_and_reject() {
        let pool = &RPC_RULES.as_ref().unwrap().pool;
        let cases = [
            (
                "chat.v1.Message",
                0,
                "ephemeral",
                Value::Bool(true),
                Value::Bool(false),
            ),
            (
                "chat.v1.Channel",
                1,
                "type",
                Value::EnumNumber(4),
                Value::EnumNumber(1),
            ),
            (
                "chat.v1.Channel",
                2,
                "name",
                Value::String("".into()),
                Value::String("valid".into()),
            ),
            (
                "chat.v1.Channel.Config",
                3,
                "persistence",
                Value::EnumNumber(1),
                Value::EnumNumber(2),
            ),
            (
                "chat.v1.ChannelModeration",
                4,
                "user_id",
                Value::String("".into()),
                Value::String("different".into()),
            ),
            (
                "chat.v1.ChannelInvite",
                6,
                "inviter_id",
                Value::String("".into()),
                Value::String("different".into()),
            ),
            (
                "chat.v1.Session",
                7,
                "expires_at",
                Value::U64(1),
                Value::U64(i64::MAX as u64),
            ),
        ];
        for (name, expression, field, invalid, valid) in cases {
            let mut message = DynamicMessage::new(pool.get_message_by_name(name).unwrap());
            message.set_field_by_name(field, invalid);
            assert!(
                !check_expression(&message, SUPPORTED_EXPRESSIONS[expression]),
                "{name}"
            );
            message.set_field_by_name(field, valid);
            assert!(
                check_expression(&message, SUPPORTED_EXPRESSIONS[expression]),
                "{name}"
            );
        }
    }

    #[test]
    fn rpc_optional_moderation_duration_matches_action() {
        let pool = &RPC_RULES.as_ref().unwrap().pool;
        let mut message = DynamicMessage::new(
            pool.get_message_by_name("chat.v1.ChannelModeration")
                .unwrap(),
        );
        for action in 0..=5 {
            message.set_field_by_name("action", Value::EnumNumber(action));
            assert!(check_expression(&message, SUPPORTED_EXPRESSIONS[5]));
        }
        message.set_field_by_name(
            "duration",
            Value::Message(DynamicMessage::new(
                pool.get_message_by_name("google.protobuf.Duration")
                    .unwrap(),
            )),
        );
        for action in 0..=5 {
            message.set_field_by_name("action", Value::EnumNumber(action));
            assert_eq!(
                check_expression(&message, SUPPORTED_EXPRESSIONS[5]),
                matches!(action, 2 | 3)
            );
        }
    }

    #[test]
    fn rpc_duration_and_repeated_minimum_are_enforced() {
        let rules = RPC_RULES.as_ref().unwrap();
        let desc = rules
            .pool
            .get_message_by_name("google.protobuf.Duration")
            .unwrap();
        let mut duration = DynamicMessage::new(desc.clone());
        for (greater, seconds, valid) in [
            (true, 0, false),
            (true, 1, true),
            (false, 10, false),
            (false, 9, true),
        ] {
            duration.set_field_by_name("seconds", Value::I64(seconds));
            duration.set_field_by_name("nanos", Value::I32(0));
            let bound = if greater { 0 } else { 10_000_000_000 };
            let result = check_value(
                &Value::Message(duration.clone()),
                &ValueRules {
                    checks: vec![Check::DurationBound(bound, greater)],
                    ..Default::default()
                },
                &Kind::Message(desc.clone()),
                &[],
            );
            assert_eq!(result.is_ok(), valid);
        }
        let mut webhook = DynamicMessage::new(
            rules
                .pool
                .get_message_by_name("chat.v1.WebhookConfig")
                .unwrap(),
        );
        webhook.set_field_by_name("url", Value::String("https://example.test".into()));
        assert!(matches!(
            rules.check_message(&webhook, &mut Vec::new()),
            Err(ContentValidationError::Violation {
                rule: "repeated.min_items",
                ..
            })
        ));
        let mut event = DynamicMessage::new(
            rules
                .pool
                .get_message_by_name("chat.v1.WebhookEvent")
                .unwrap(),
        );
        event.set_field_by_name("type", Value::EnumNumber(1));
        webhook.set_field_by_name("events", Value::List(vec![Value::Message(event)]));
        webhook.set_field_by_name("timeout_seconds", Value::U32(1));
        assert!(rules.check_message(&webhook, &mut Vec::new()).is_ok());
    }
    /// Every rule content.proto declares compiles into a check. A rule kind the
    /// evaluator does not understand would otherwise go silently unenforced.
    #[test]
    fn every_rpc_response_rule_is_supported() {
        if let Err(e) = compile_rpc() {
            panic!("{e}");
        }
    }

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
