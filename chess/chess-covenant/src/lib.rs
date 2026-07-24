pub mod cozy_bridge;
pub mod indexer;
pub mod observer;
pub mod orchestrator;
pub mod protocol_move;
pub mod txdecode;

use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

fn source_cache() -> &'static Mutex<std::collections::HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

pub fn load_contract_source(path: &str) -> String {
    {
        let cache = source_cache().lock().expect("source cache mutex poisoned");
        if let Some(source) = cache.get(path) {
            return source.clone();
        }
    }

    let source = fs::read_to_string(path).unwrap_or_else(|err| panic!("failed to read {path}: {err}"));
    let expanded = preprocess_contract_source(path, &source);

    let mut cache = source_cache().lock().expect("source cache mutex poisoned");
    cache.insert(path.to_string(), expanded.clone());
    expanded
}

fn preprocess_contract_source(path: &str, source: &str) -> String {
    let mut consts_file = None;
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("// @consts ") {
            consts_file = Some(rest.trim().to_string());
            break;
        }
    }

    let Some(consts_file) = consts_file else {
        return source.to_string();
    };

    let consts_path = Path::new(path).parent().unwrap_or_else(|| Path::new(".")).join(&consts_file);
    let consts_source =
        fs::read_to_string(&consts_path).unwrap_or_else(|err| panic!("failed to read {}: {err}", consts_path.display()));
    let constants = parse_constants(&consts_source);

    let mut expanded = source.replace(&format!("// @consts {consts_file}"), &format!("// consts expanded from {consts_file}"));
    for (name, value) in constants {
        expanded = replace_identifier_tokens(&expanded, &name, &value);
    }
    expanded
}

fn parse_constants(source: &str) -> Vec<(String, String)> {
    let mut constants = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("int constant ") {
            let Some((name, value_with_semi)) = rest.split_once(" = ") else {
                continue;
            };
            let Some(value) = value_with_semi.strip_suffix(';') else {
                continue;
            };
            constants.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    constants
}

fn replace_identifier_tokens(source: &str, name: &str, value: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();

    while let Some(ch) = chars.next() {
        if is_ident_start(ch) {
            let mut token = String::new();
            token.push(ch);
            while let Some(next) = chars.peek().copied() {
                if is_ident_continue(next) {
                    token.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            if token == name {
                out.push_str(value);
            } else {
                out.push_str(&token);
            }
        } else {
            out.push(ch);
        }
    }

    out
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

pub fn mux_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_mux.sil")
}

pub fn settle_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_settle.sil")
}

pub fn league_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/league.sil")
}

pub fn player_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/player.sil")
}

pub fn pawn_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_pawn.sil")
}

pub fn knight_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_knight.sil")
}

pub fn vert_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_vert.sil")
}

pub fn horiz_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_horiz.sil")
}

pub fn diag_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_diag.sil")
}

pub fn king_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_king.sil")
}

pub fn castle_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_castle.sil")
}

pub fn castle_challenge_contract_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../baseline/sil/chess_castle_challenge.sil")
}
