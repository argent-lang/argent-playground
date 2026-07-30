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
            expected_script_len: 482,
            expected_instruction_count: 283,
            expected_charged_op_count: 206,
            baseline_script_len: 488,
            baseline_instruction_count: 289,
            baseline_charged_op_count: 213,
        },
        SizeSnapshot {
            actor: "Player",
            expected_script_len: 3510,
            expected_instruction_count: 2542,
            expected_charged_op_count: 1644,
            baseline_script_len: 3456,
            baseline_instruction_count: 2550,
            baseline_charged_op_count: 1660,
        },
        SizeSnapshot {
            actor: "Mux",
            expected_script_len: 1791,
            expected_instruction_count: 1059,
            expected_charged_op_count: 720,
            baseline_script_len: 1754,
            baseline_instruction_count: 1086,
            baseline_charged_op_count: 736,
        },
        SizeSnapshot {
            actor: "Settle",
            expected_script_len: 2833,
            expected_instruction_count: 2072,
            expected_charged_op_count: 1363,
            baseline_script_len: 2656,
            baseline_instruction_count: 2068,
            baseline_charged_op_count: 1347,
        },
        SizeSnapshot {
            actor: "Pawn",
            expected_script_len: 1856,
            expected_instruction_count: 1179,
            expected_charged_op_count: 776,
            baseline_script_len: 1972,
            baseline_instruction_count: 1332,
            baseline_charged_op_count: 872,
        },
        SizeSnapshot {
            actor: "Knight",
            expected_script_len: 1444,
            expected_instruction_count: 800,
            expected_charged_op_count: 529,
            baseline_script_len: 1496,
            baseline_instruction_count: 891,
            baseline_charged_op_count: 594,
        },
        SizeSnapshot {
            actor: "Vert",
            expected_script_len: 1993,
            expected_instruction_count: 1318,
            expected_charged_op_count: 903,
            baseline_script_len: 2104,
            baseline_instruction_count: 1469,
            baseline_charged_op_count: 1011,
        },
        SizeSnapshot {
            actor: "Horiz",
            expected_script_len: 1993,
            expected_instruction_count: 1318,
            expected_charged_op_count: 903,
            baseline_script_len: 2104,
            baseline_instruction_count: 1469,
            baseline_charged_op_count: 1011,
        },
        SizeSnapshot {
            actor: "Diag",
            expected_script_len: 1996,
            expected_instruction_count: 1331,
            expected_charged_op_count: 918,
            baseline_script_len: 2071,
            baseline_instruction_count: 1443,
            baseline_charged_op_count: 993,
        },
        SizeSnapshot {
            actor: "King",
            expected_script_len: 1512,
            expected_instruction_count: 851,
            expected_charged_op_count: 564,
            baseline_script_len: 1582,
            baseline_instruction_count: 958,
            baseline_charged_op_count: 637,
        },
        SizeSnapshot {
            actor: "Castle",
            expected_script_len: 1545,
            expected_instruction_count: 896,
            expected_charged_op_count: 589,
            baseline_script_len: 1630,
            baseline_instruction_count: 1019,
            baseline_charged_op_count: 670,
        },
        SizeSnapshot {
            actor: "CastleChallengePrep",
            expected_script_len: 1696,
            expected_instruction_count: 1039,
            expected_charged_op_count: 685,
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
