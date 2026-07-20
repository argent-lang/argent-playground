use std::collections::BTreeMap;

use kaspa_txscript::deserialize_i64;
use kaspa_txscript::opcodes::codes::{
    Op0 as OP_0, Op1 as OP_1, Op16 as OP_16, Op1Negate as OP_1_NEGATE, OpPushData1 as OP_PUSHDATA1, OpPushData2 as OP_PUSHDATA2,
    OpPushData4 as OP_PUSHDATA4,
};
use silverscript_lang::ast::{parse_type_ref, StructAst, StructFieldAst, TypeBase, TypeRef};
use silverscript_lang::compiler::{CompiledContract, FunctionAbiEntry};

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
    pub abi: Vec<FunctionAbiEntry>,
    pub without_selector: bool,
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
    pub state_layout_len: usize,
    fields: Vec<(String, TypeRef)>,
    structs: BTreeMap<String, Vec<(String, TypeRef)>>,
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
}

impl ContractTemplate {
    pub fn from_compiled(compiled: &CompiledContract<'_>) -> Self {
        let start = compiled.state_layout.start;
        let end = start + compiled.state_layout.len;
        let prefix = compiled.script[..start].to_vec();
        let suffix = compiled.script[end..].to_vec();
        let fields = compiled.ast.fields.iter().map(|field| (field.name.clone(), field.type_ref.clone())).collect();
        let structs = compiled.ast.structs.iter().map(struct_spec).collect::<BTreeMap<_, _>>();
        Self {
            contract_name: compiled.contract_name.clone(),
            abi: compiled.abi.clone(),
            without_selector: compiled.without_selector,
            prefix,
            suffix,
            state_layout_len: compiled.state_layout.len,
            fields,
            structs,
        }
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
        let (function, selector, args_slice) = if self.without_selector {
            let entry =
                self.abi.first().ok_or_else(|| DecodeError::UnknownSelector { contract: self.contract_name.clone(), selector: 0 })?;
            (entry, None, call_items)
        } else {
            let selector_item = call_items.last().ok_or(DecodeError::UnexpectedEof)?;
            let selector = decode_script_num(selector_item)?;
            let entry = self
                .abi
                .get(selector as usize)
                .ok_or_else(|| DecodeError::UnknownSelector { contract: self.contract_name.clone(), selector })?;
            (entry, Some(selector), &call_items[..call_items.len() - 1])
        };

        if args_slice.len() != function.inputs.len() {
            return Err(DecodeError::ArgumentCountMismatch {
                contract: self.contract_name.clone(),
                function: function.name.clone(),
                expected: function.inputs.len(),
                actual: args_slice.len(),
            });
        }

        let mut args = Vec::with_capacity(function.inputs.len());
        for (input, raw) in function.inputs.iter().zip(args_slice.iter()) {
            let type_ref = parse_type_ref(&input.type_name).map_err(|_| DecodeError::UnsupportedType(input.type_name.clone()))?;
            let value = decode_value_from_bytes(raw, &type_ref, &self.structs, DecodeMode::SigScript)?;
            args.push(DecodedArg { name: input.name.clone(), type_name: input.type_name.clone(), value });
        }

        Ok(DecodedCall { function: function.name.clone(), selector, args })
    }
}

fn struct_spec(item: &StructAst<'_>) -> (String, Vec<(String, TypeRef)>) {
    let fields = item.fields.iter().map(struct_field_spec).collect::<Vec<_>>();
    (item.name.clone(), fields)
}

fn struct_field_spec(field: &StructFieldAst<'_>) -> (String, TypeRef) {
    (field.name.clone(), field.type_ref.clone())
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
    type_ref: &TypeRef,
    structs: &BTreeMap<String, Vec<(String, TypeRef)>>,
    mode: DecodeMode,
) -> Result<DecodeValue, DecodeError> {
    if let TypeBase::Custom(name) = &type_ref.base {
        let fields = structs.get(name).ok_or_else(|| DecodeError::UnsupportedType(type_ref.type_name()))?;
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

    if !type_ref.array_dims.is_empty() {
        return decode_array_value(bytes, type_ref);
    }

    match type_ref.base {
        TypeBase::Int => Ok(DecodeValue::Int(match mode {
            DecodeMode::State => decode_fixed_i64(bytes)?,
            DecodeMode::SigScript => decode_script_num(bytes)?,
        })),
        TypeBase::Bool => match bytes {
            [] if mode == DecodeMode::SigScript => Ok(DecodeValue::Bool(false)),
            [0] => Ok(DecodeValue::Bool(false)),
            [1] => Ok(DecodeValue::Bool(true)),
            _ => Err(DecodeError::InvalidBoolEncoding),
        },
        TypeBase::Byte | TypeBase::String | TypeBase::Pubkey | TypeBase::Sig | TypeBase::Datasig => {
            Ok(DecodeValue::Bytes(bytes.to_vec()))
        }
        TypeBase::Custom(_) => unreachable!("custom types handled above"),
    }
}

fn decode_fixed_i64(bytes: &[u8]) -> Result<i64, DecodeError> {
    if bytes.len() != 8 {
        return Err(DecodeError::InvalidIntegerEncoding);
    }
    deserialize_i64(bytes, false).map_err(|_| DecodeError::InvalidIntegerEncoding)
}

fn decode_array_value(bytes: &[u8], type_ref: &TypeRef) -> Result<DecodeValue, DecodeError> {
    let element_type = type_ref.array_element_type().ok_or_else(|| DecodeError::UnsupportedType(type_ref.type_name()))?;
    if element_type.base == TypeBase::Byte {
        return Ok(DecodeValue::Bytes(bytes.to_vec()));
    }
    Err(DecodeError::UnsupportedType(type_ref.type_name()))
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
