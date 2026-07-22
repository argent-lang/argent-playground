use std::collections::BTreeMap;

use argent_artifact::{Artifact, TypeArtifact};
use kaspa_txscript::deserialize_i64;
use kaspa_txscript::opcodes::codes::{
    Op0 as OP_0, Op1 as OP_1, Op16 as OP_16, Op1Negate as OP_1_NEGATE, OpPushData1 as OP_PUSHDATA1, OpPushData2 as OP_PUSHDATA2,
    OpPushData4 as OP_PUSHDATA4,
};
use silverscript_lang::ast::{ArrayDim, StructAst, StructFieldAst, TypeBase, TypeRef};
use silverscript_lang::compiler::CompiledContract;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeValue {
    Int(i64),
    Bool(bool),
    Bytes(Vec<u8>),
    Struct(DecodedObject),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedField {
    pub name: String,
    pub value: DecodeValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DecodedObject {
    pub fields: Vec<DecodedField>,
}

impl DecodedObject {
    pub fn get(&self, name: &str) -> Option<&DecodeValue> {
        self.fields.iter().find(|field| field.name == name).map(|field| &field.value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedArg {
    pub name: String,
    pub type_name: String,
    pub value: DecodeValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedCall {
    pub function: String,
    pub selector: Option<i64>,
    pub args: Vec<DecodedArg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct P2shCall {
    pub stack_items: Vec<Vec<u8>>,
    pub redeem_script: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ContractTemplate {
    pub contract_name: String,
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
    pub state_layout_len: usize,
    entries: Vec<EntryTemplate>,
    fields: Vec<(String, TypeArtifact)>,
    structs: BTreeMap<String, Vec<(String, TypeArtifact)>>,
}

#[derive(Debug, Clone)]
struct EntryTemplate {
    name: String,
    selector: Option<i64>,
    inputs: Vec<(String, TypeArtifact)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeMode {
    State,
    SigScript,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum DecodeError {
    #[error("script contains non-push opcode 0x{0:02x}")]
    NonPushOpcode(u8),
    #[error("script ended unexpectedly")]
    UnexpectedEof,
    #[error("missing redeem script push in P2SH signature script")]
    MissingRedeemScript,
    #[error("redeem script does not match contract template {0}")]
    TemplateMismatch(String),
    #[error("unknown entrypoint selector {selector} for contract {contract}")]
    UnknownSelector { contract: String, selector: i64 },
    #[error("sigscript argument count mismatch for {contract}.{function}: expected {expected}, got {actual}")]
    ArgumentCountMismatch { contract: String, function: String, expected: usize, actual: usize },
    #[error("unsupported type {0}")]
    UnsupportedType(String),
    #[error("invalid integer encoding")]
    InvalidIntegerEncoding,
    #[error("invalid bool encoding")]
    InvalidBoolEncoding,
    #[error("state field count mismatch for {contract}: expected {expected}, got {actual}")]
    StateFieldCountMismatch { contract: String, expected: usize, actual: usize },
    #[error("unknown contract {0}")]
    UnknownContract(String),
    #[error("invalid hex in compiled contract {0}")]
    InvalidHex(String),
}

impl ContractTemplate {
    pub fn from_compiled(compiled: &CompiledContract<'_>) -> Self {
        let start = compiled.state_layout.start;
        let end = start + compiled.state_layout.len;
        let prefix = compiled.script[..start].to_vec();
        let suffix = compiled.script[end..].to_vec();
        let fields = compiled.ast.fields.iter().map(|field| (field.name.clone(), type_artifact(&field.type_ref))).collect();
        let structs = compiled.ast.structs.iter().map(struct_spec).collect::<BTreeMap<_, _>>();
        let entries = compiled
            .abi
            .iter()
            .enumerate()
            .map(|(index, entry)| EntryTemplate {
                name: entry.name.clone(),
                selector: (!compiled.without_selector).then_some(index as i64),
                inputs: entry
                    .inputs
                    .iter()
                    .map(|input| {
                        let ty = silverscript_lang::ast::parse_type_ref(&input.type_name)
                            .expect("compiled ABI contains valid Silverscript types");
                        (input.name.clone(), type_artifact(&ty))
                    })
                    .collect(),
            })
            .collect();
        Self {
            contract_name: compiled.contract_name.clone(),
            prefix,
            suffix,
            state_layout_len: compiled.state_layout.len,
            entries,
            fields,
            structs,
        }
    }

    pub fn from_artifact(artifact: &Artifact, contract_name: &str) -> Result<Self, DecodeError> {
        let contract =
            artifact.sil_abi.contract(contract_name).ok_or_else(|| DecodeError::UnknownContract(contract_name.to_string()))?;
        let prefix = decode_hex(&contract.compiled.template.prefix_hex)?;
        let suffix = decode_hex(&contract.compiled.template.suffix_hex)?;
        let fields = contract.runtime_state.fields.iter().map(|field| (field.name.clone(), field.ty.clone())).collect();
        let structs = artifact
            .sil_abi
            .states
            .iter()
            .map(|state| (state.name.clone(), state.fields.iter().map(|field| (field.name.clone(), field.ty.clone())).collect()))
            .collect();
        let entries = contract
            .entries
            .iter()
            .map(|entry| EntryTemplate {
                name: entry.name.clone(),
                selector: entry.selector,
                inputs: entry.params.iter().map(|param| (param.name.clone(), param.ty.clone())).collect(),
            })
            .collect();
        Ok(Self {
            contract_name: contract.name.clone(),
            prefix,
            suffix,
            state_layout_len: contract.compiled.state_span.len,
            entries,
            fields,
            structs,
        })
    }

    pub fn matches_redeem_script(&self, redeem_script: &[u8]) -> bool {
        redeem_script.len() == self.prefix.len() + self.state_layout_len + self.suffix.len()
            && redeem_script.starts_with(&self.prefix)
            && redeem_script.ends_with(&self.suffix)
    }

    pub fn decode_state(&self, redeem_script: &[u8]) -> Result<DecodedObject, DecodeError> {
        if !self.matches_redeem_script(redeem_script) {
            return Err(DecodeError::TemplateMismatch(self.contract_name.clone()));
        }
        let state_start = self.prefix.len();
        let state_end = state_start + self.state_layout_len;
        let state_bytes = &redeem_script[state_start..state_end];
        let items = parse_push_only_script(state_bytes)?;
        if items.len() != self.fields.len() {
            return Err(DecodeError::StateFieldCountMismatch {
                contract: self.contract_name.clone(),
                expected: self.fields.len(),
                actual: items.len(),
            });
        }

        let mut fields = Vec::with_capacity(self.fields.len());
        for ((name, type_ref), item) in self.fields.iter().zip(items.iter()) {
            let value = decode_value_from_bytes(item, type_ref, &self.structs, DecodeMode::State)?;
            fields.push(DecodedField { name: name.clone(), value });
        }
        Ok(DecodedObject { fields })
    }

    pub fn decode_call(&self, call_items: &[Vec<u8>]) -> Result<DecodedCall, DecodeError> {
        let selectorless = self.entries.iter().find(|entry| entry.selector.is_none());
        let (entry, selector, args_slice) = if let Some(entry) = selectorless {
            (entry, None, call_items)
        } else {
            let selector_item = call_items.last().ok_or(DecodeError::UnexpectedEof)?;
            let selector = decode_script_num(selector_item)?;
            let entry = self
                .entries
                .iter()
                .find(|entry| entry.selector == Some(selector))
                .ok_or_else(|| DecodeError::UnknownSelector { contract: self.contract_name.clone(), selector })?;
            (entry, Some(selector), &call_items[..call_items.len() - 1])
        };

        if args_slice.len() != entry.inputs.len() {
            return Err(DecodeError::ArgumentCountMismatch {
                contract: self.contract_name.clone(),
                function: entry.name.clone(),
                expected: entry.inputs.len(),
                actual: args_slice.len(),
            });
        }

        let mut args = Vec::with_capacity(entry.inputs.len());
        for ((name, ty), raw) in entry.inputs.iter().zip(args_slice.iter()) {
            let value = decode_value_from_bytes(raw, ty, &self.structs, DecodeMode::SigScript)?;
            args.push(DecodedArg { name: name.clone(), type_name: type_name(ty), value });
        }

        Ok(DecodedCall { function: entry.name.clone(), selector, args })
    }
}

fn struct_spec(item: &StructAst<'_>) -> (String, Vec<(String, TypeArtifact)>) {
    let fields = item.fields.iter().map(struct_field_spec).collect::<Vec<_>>();
    (item.name.clone(), fields)
}

fn struct_field_spec(field: &StructFieldAst<'_>) -> (String, TypeArtifact) {
    (field.name.clone(), type_artifact(&field.type_ref))
}

pub fn decode_p2sh_call(signature_script: &[u8]) -> Result<P2shCall, DecodeError> {
    let items = parse_push_only_script(signature_script)?;
    let (redeem_script, stack_items) = items.split_last().ok_or(DecodeError::MissingRedeemScript)?;
    Ok(P2shCall { stack_items: stack_items.to_vec(), redeem_script: redeem_script.clone() })
}

pub fn parse_push_only_script(script: &[u8]) -> Result<Vec<Vec<u8>>, DecodeError> {
    let mut items = Vec::new();
    let mut offset = 0usize;
    while offset < script.len() {
        let opcode = script[offset];
        offset += 1;
        match opcode {
            OP_0 => items.push(Vec::new()),
            OP_1_NEGATE => items.push(vec![0x81]),
            OP_1..=OP_16 => items.push(vec![opcode - OP_1 + 1]),
            1..=75 => {
                let len = opcode as usize;
                if offset + len > script.len() {
                    return Err(DecodeError::UnexpectedEof);
                }
                items.push(script[offset..offset + len].to_vec());
                offset += len;
            }
            OP_PUSHDATA1 => {
                if offset >= script.len() {
                    return Err(DecodeError::UnexpectedEof);
                }
                let len = script[offset] as usize;
                offset += 1;
                if offset + len > script.len() {
                    return Err(DecodeError::UnexpectedEof);
                }
                items.push(script[offset..offset + len].to_vec());
                offset += len;
            }
            OP_PUSHDATA2 => {
                if offset + 2 > script.len() {
                    return Err(DecodeError::UnexpectedEof);
                }
                let len = u16::from_le_bytes([script[offset], script[offset + 1]]) as usize;
                offset += 2;
                if offset + len > script.len() {
                    return Err(DecodeError::UnexpectedEof);
                }
                items.push(script[offset..offset + len].to_vec());
                offset += len;
            }
            OP_PUSHDATA4 => {
                if offset + 4 > script.len() {
                    return Err(DecodeError::UnexpectedEof);
                }
                let len = u32::from_le_bytes([script[offset], script[offset + 1], script[offset + 2], script[offset + 3]]) as usize;
                offset += 4;
                if offset + len > script.len() {
                    return Err(DecodeError::UnexpectedEof);
                }
                items.push(script[offset..offset + len].to_vec());
                offset += len;
            }
            other => return Err(DecodeError::NonPushOpcode(other)),
        }
    }
    Ok(items)
}

fn decode_value_from_bytes(
    bytes: &[u8],
    ty: &TypeArtifact,
    structs: &BTreeMap<String, Vec<(String, TypeArtifact)>>,
    mode: DecodeMode,
) -> Result<DecodeValue, DecodeError> {
    if let TypeArtifact::Struct { name } = ty {
        let fields = structs.get(name).ok_or_else(|| DecodeError::UnsupportedType(name.clone()))?;
        let items = parse_push_only_script(bytes)?;
        if items.len() != fields.len() {
            return Err(DecodeError::StateFieldCountMismatch { contract: name.clone(), expected: fields.len(), actual: items.len() });
        }
        let mut decoded = Vec::with_capacity(fields.len());
        for ((field_name, field_type), item) in fields.iter().zip(items.iter()) {
            let value = decode_value_from_bytes(item, field_type, structs, mode)?;
            decoded.push(DecodedField { name: field_name.clone(), value });
        }
        return Ok(DecodeValue::Struct(DecodedObject { fields: decoded }));
    }

    match ty {
        TypeArtifact::Int => Ok(DecodeValue::Int(match mode {
            DecodeMode::State => decode_fixed_i64(bytes)?,
            DecodeMode::SigScript => decode_script_num(bytes)?,
        })),
        TypeArtifact::Bool => match bytes {
            [] if mode == DecodeMode::SigScript => Ok(DecodeValue::Bool(false)),
            [0] => Ok(DecodeValue::Bool(false)),
            [1] => Ok(DecodeValue::Bool(true)),
            _ => Err(DecodeError::InvalidBoolEncoding),
        },
        TypeArtifact::Byte
        | TypeArtifact::Bytes
        | TypeArtifact::Text
        | TypeArtifact::Pubkey
        | TypeArtifact::Sig
        | TypeArtifact::Datasig
        | TypeArtifact::FixedBytes { .. } => Ok(DecodeValue::Bytes(bytes.to_vec())),
        TypeArtifact::FixedArray { item, .. } | TypeArtifact::DynamicArray { item } if matches!(item.as_ref(), TypeArtifact::Byte) => {
            Ok(DecodeValue::Bytes(bytes.to_vec()))
        }
        TypeArtifact::FixedArray { .. } | TypeArtifact::DynamicArray { .. } => Err(DecodeError::UnsupportedType(type_name(ty))),
        TypeArtifact::Struct { .. } => unreachable!("struct types handled above"),
    }
}

fn decode_fixed_i64(bytes: &[u8]) -> Result<i64, DecodeError> {
    if bytes.len() != 8 {
        return Err(DecodeError::InvalidIntegerEncoding);
    }
    deserialize_i64(bytes, false).map_err(|_| DecodeError::InvalidIntegerEncoding)
}

fn type_artifact(ty: &TypeRef) -> TypeArtifact {
    let mut artifact = match &ty.base {
        TypeBase::Int => TypeArtifact::Int,
        TypeBase::Bool => TypeArtifact::Bool,
        TypeBase::String => TypeArtifact::Text,
        TypeBase::Pubkey => TypeArtifact::Pubkey,
        TypeBase::Sig => TypeArtifact::Sig,
        TypeBase::Datasig => TypeArtifact::Datasig,
        TypeBase::Byte => TypeArtifact::Byte,
        TypeBase::Custom(name) => TypeArtifact::Struct { name: name.clone() },
    };
    for dim in &ty.array_dims {
        artifact = match dim {
            ArrayDim::Dynamic if matches!(artifact, TypeArtifact::Byte) => TypeArtifact::Bytes,
            ArrayDim::Fixed(len) if matches!(artifact, TypeArtifact::Byte) => TypeArtifact::FixedBytes { len: *len },
            ArrayDim::Dynamic => TypeArtifact::DynamicArray { item: Box::new(artifact) },
            ArrayDim::Fixed(len) => TypeArtifact::FixedArray { item: Box::new(artifact), len: *len },
            ArrayDim::Inferred | ArrayDim::Constant(_) => TypeArtifact::DynamicArray { item: Box::new(artifact) },
        };
    }
    artifact
}

fn type_name(ty: &TypeArtifact) -> String {
    match ty {
        TypeArtifact::Int => "int".to_string(),
        TypeArtifact::Bool => "bool".to_string(),
        TypeArtifact::Byte => "byte".to_string(),
        TypeArtifact::Bytes => "byte[]".to_string(),
        TypeArtifact::Text => "string".to_string(),
        TypeArtifact::Pubkey => "pubkey".to_string(),
        TypeArtifact::Sig => "sig".to_string(),
        TypeArtifact::Datasig => "datasig".to_string(),
        TypeArtifact::FixedBytes { len } => format!("byte[{len}]"),
        TypeArtifact::FixedArray { item, len } => format!("{}[{len}]", type_name(item)),
        TypeArtifact::DynamicArray { item } => format!("{}[]", type_name(item)),
        TypeArtifact::Struct { name } => name.clone(),
    }
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, DecodeError> {
    if !hex.len().is_multiple_of(2) {
        return Err(DecodeError::InvalidHex(hex.to_string()));
    }
    (0..hex.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).map_err(|_| DecodeError::InvalidHex(hex.to_string())))
        .collect()
}

pub fn decode_script_num(bytes: &[u8]) -> Result<i64, DecodeError> {
    if bytes.len() > 8 {
        return Err(DecodeError::InvalidIntegerEncoding);
    }
    if bytes.is_empty() {
        return Ok(0);
    }
    if bytes[bytes.len() - 1] & 0x7f == 0 && (bytes.len() == 1 || bytes[bytes.len() - 2] & 0x80 == 0) {
        return Err(DecodeError::InvalidIntegerEncoding);
    }
    let msb = bytes[bytes.len() - 1];
    let sign = 1 - 2 * ((msb >> 7) as i64);
    let first = (msb & 0x7f) as i64;
    let value = bytes[..bytes.len() - 1].iter().rev().fold(first, |acc, byte| (acc << 8) + i64::from(*byte));
    Ok(value * sign)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_contract_state_from_argent_artifact() {
        let artifact: Artifact =
            serde_json::from_str(include_str!("../../build/argent/artifact.json")).expect("pinned chess artifact deserializes");
        let contract = artifact.sil_abi.contract("League").expect("League ABI exists");
        let template = ContractTemplate::from_artifact(&artifact, "League").expect("League template loads");
        let script = decode_hex(&contract.compiled.script_hex).expect("compiled League script is valid hex");

        assert!(template.matches_redeem_script(&script));
        let state = template.decode_state(&script).expect("canonical League state decodes");
        assert_eq!(state.fields.len(), contract.runtime_state.fields.len());
        assert_eq!(state.fields.last().expect("admin field").name, "admin");
    }
}
