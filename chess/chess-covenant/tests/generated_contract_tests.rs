use argent_artifact::Artifact;
use kaspa_consensus_core::hashing::sighash::SigHashReusedValuesUnsync;
use kaspa_consensus_core::tx::PopulatedTransaction;
use kaspa_txscript::parse_script;

struct SizeSnapshot {
    actor: &'static str,
    expected_script_len: usize,
    expected_instruction_count: usize,
    expected_charged_op_count: usize,
    baseline_script_len: usize,
    baseline_instruction_count: usize,
    baseline_charged_op_count: usize,
}

fn artifact() -> Artifact {
    let artifact: Artifact =
        serde_json::from_str(include_str!("../../build/artifact.json")).expect("pinned chess artifact deserializes");
    artifact.check_schema_version().expect("chess artifact schema is supported");
    artifact.verify_id().expect("chess artifact id verifies");
    artifact.verify_template_plan().expect("chess template plan verifies");
    artifact
}

fn decode_hex(value: &str) -> Vec<u8> {
    assert_eq!(value.len() % 2, 0, "hex value must contain complete bytes");
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|digits| {
            let text = std::str::from_utf8(digits).expect("hex digits are ASCII");
            u8::from_str_radix(text, 16).expect("artifact contains valid hex")
        })
        .collect()
}

fn script_op_counts(script: &[u8]) -> (usize, usize) {
    let mut instruction_count = 0;
    let mut charged_op_count = 0;
    for opcode in parse_script::<PopulatedTransaction<'_>, SigHashReusedValuesUnsync>(script) {
        let opcode = opcode.expect("compiled script parses");
        instruction_count += 1;
        if !opcode.is_push_opcode() {
            charged_op_count += 1;
        }
    }
    (instruction_count, charged_op_count)
}

fn size_snapshots() -> [SizeSnapshot; 12] {
    // Baseline values are frozen measurements of the handwritten contracts.
    // The test reads only the generated artifact.
    [
        SizeSnapshot {
            actor: "League",
            expected_script_len: 488,
            expected_instruction_count: 289,
            expected_charged_op_count: 213,
            baseline_script_len: 488,
            baseline_instruction_count: 289,
            baseline_charged_op_count: 213,
        },
        SizeSnapshot {
            actor: "Player",
            expected_script_len: 3644,
            expected_instruction_count: 2672,
            expected_charged_op_count: 1733,
            baseline_script_len: 3456,
            baseline_instruction_count: 2550,
            baseline_charged_op_count: 1660,
        },
        SizeSnapshot {
            actor: "Mux",
            expected_script_len: 1889,
            expected_instruction_count: 1155,
            expected_charged_op_count: 793,
            baseline_script_len: 1754,
            baseline_instruction_count: 1086,
            baseline_charged_op_count: 736,
        },
        SizeSnapshot {
            actor: "Settle",
            expected_script_len: 2969,
            expected_instruction_count: 2212,
            expected_charged_op_count: 1450,
            baseline_script_len: 2656,
            baseline_instruction_count: 2068,
            baseline_charged_op_count: 1347,
        },
        SizeSnapshot {
            actor: "Pawn",
            expected_script_len: 2003,
            expected_instruction_count: 1327,
            expected_charged_op_count: 863,
            baseline_script_len: 1972,
            baseline_instruction_count: 1332,
            baseline_charged_op_count: 872,
        },
        SizeSnapshot {
            actor: "Knight",
            expected_script_len: 1529,
            expected_instruction_count: 886,
            expected_charged_op_count: 585,
            baseline_script_len: 1496,
            baseline_instruction_count: 891,
            baseline_charged_op_count: 594,
        },
        SizeSnapshot {
            actor: "Vert",
            expected_script_len: 2100,
            expected_instruction_count: 1426,
            expected_charged_op_count: 970,
            baseline_script_len: 2104,
            baseline_instruction_count: 1469,
            baseline_charged_op_count: 1011,
        },
        SizeSnapshot {
            actor: "Horiz",
            expected_script_len: 2100,
            expected_instruction_count: 1426,
            expected_charged_op_count: 970,
            baseline_script_len: 2104,
            baseline_instruction_count: 1469,
            baseline_charged_op_count: 1011,
        },
        SizeSnapshot {
            actor: "Diag",
            expected_script_len: 2101,
            expected_instruction_count: 1437,
            expected_charged_op_count: 984,
            baseline_script_len: 2071,
            baseline_instruction_count: 1443,
            baseline_charged_op_count: 993,
        },
        SizeSnapshot {
            actor: "King",
            expected_script_len: 1613,
            expected_instruction_count: 953,
            expected_charged_op_count: 628,
            baseline_script_len: 1582,
            baseline_instruction_count: 958,
            baseline_charged_op_count: 637,
        },
        SizeSnapshot {
            actor: "Castle",
            expected_script_len: 1647,
            expected_instruction_count: 998,
            expected_charged_op_count: 653,
            baseline_script_len: 1630,
            baseline_instruction_count: 1019,
            baseline_charged_op_count: 670,
        },
        SizeSnapshot {
            actor: "CastleChallengePrep",
            expected_script_len: 1809,
            expected_instruction_count: 1152,
            expected_charged_op_count: 754,
            baseline_script_len: 1729,
            baseline_instruction_count: 1120,
            baseline_charged_op_count: 733,
        },
    ]
}

#[test]
fn generated_artifact_contains_the_complete_chess_application() {
    let artifact = artifact();
    let actors = artifact.argent.actors.iter().map(|actor| actor.name.as_str()).collect::<Vec<_>>();
    let contracts = artifact.sil_abi.contracts.iter().map(|contract| contract.name.as_str()).collect::<Vec<_>>();
    let expected =
        ["League", "Player", "Mux", "Pawn", "Knight", "Vert", "Horiz", "Diag", "King", "Castle", "CastleChallengePrep", "Settle"];

    assert_eq!(actors, expected);
    assert_eq!(contracts, expected);
}

#[test]
fn generated_contract_sizes_match_snapshots() {
    let artifact = artifact();
    let mut actual = Vec::new();
    for snapshot in size_snapshots() {
        let contract = artifact.sil_abi.contract(snapshot.actor).expect("snapshot actor has a compiled contract");
        let script = decode_hex(&contract.compiled.script_hex);
        let (instruction_count, charged_op_count) = script_op_counts(&script);
        actual.push((snapshot, script.len(), instruction_count, charged_op_count));
    }

    for (snapshot, script_len, instruction_count, charged_op_count) in &actual {
        eprintln!(
            "{}: generated={}/{}/{} baseline={}/{}/{} delta={:+}/{:+}/{:+}",
            snapshot.actor,
            script_len,
            instruction_count,
            charged_op_count,
            snapshot.baseline_script_len,
            snapshot.baseline_instruction_count,
            snapshot.baseline_charged_op_count,
            *script_len as isize - snapshot.baseline_script_len as isize,
            *instruction_count as isize - snapshot.baseline_instruction_count as isize,
            *charged_op_count as isize - snapshot.baseline_charged_op_count as isize
        );
    }

    for (snapshot, script_len, instruction_count, charged_op_count) in actual {
        assert_eq!(script_len, snapshot.expected_script_len, "{} script length changed", snapshot.actor);
        assert_eq!(instruction_count, snapshot.expected_instruction_count, "{} instruction count changed", snapshot.actor);
        assert_eq!(charged_op_count, snapshot.expected_charged_op_count, "{} charged operation count changed", snapshot.actor);
    }
}
