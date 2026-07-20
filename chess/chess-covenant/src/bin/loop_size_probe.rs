use std::env;
use std::time::Instant;

use silverscript_lang::ast::Expr;
use silverscript_lang::compiler::{compile_contract, CompileOptions};

#[derive(Clone, Copy)]
enum ProbeMode {
    EmptyIf,
    BoardIf,
    CounterOnly,
    CounterUnderIf,
    TwoCountersUnderIf,
    IfElseTwoUpdates,
    NestedIfUpdate,
    CounterPlusMathUnderIf,
    RequireInLoop,
    GuardedPairAdvance,
    GuardedPairAdvanceWithBool,
    GuardedPairAdvanceWithIndex,
    GuardedPairAdvanceWithBoardCheck,
    RookHelperExact,
    RookHelperNoBoardCheck,
    HelperSingleVar,
    HelperSingleVarInlineCond,
    HelperSingleVarNoClear,
    HelperSingleConst,
    EntrypointSingleConst,
}

impl ProbeMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "empty-if" => Some(Self::EmptyIf),
            "board-if" => Some(Self::BoardIf),
            "counter-only" => Some(Self::CounterOnly),
            "counter-if" => Some(Self::CounterUnderIf),
            "two-counters-if" => Some(Self::TwoCountersUnderIf),
            "if-else-two" => Some(Self::IfElseTwoUpdates),
            "nested-if" => Some(Self::NestedIfUpdate),
            "counter-math-if" => Some(Self::CounterPlusMathUnderIf),
            "require-in-loop" => Some(Self::RequireInLoop),
            "guarded-pair" => Some(Self::GuardedPairAdvance),
            "guarded-pair-bool" => Some(Self::GuardedPairAdvanceWithBool),
            "guarded-pair-index" => Some(Self::GuardedPairAdvanceWithIndex),
            "guarded-pair-board" => Some(Self::GuardedPairAdvanceWithBoardCheck),
            "rook-helper" => Some(Self::RookHelperExact),
            "rook-helper-noboard" => Some(Self::RookHelperNoBoardCheck),
            "helper-single" => Some(Self::HelperSingleVar),
            "helper-single-inline" => Some(Self::HelperSingleVarInlineCond),
            "helper-single-noclear" => Some(Self::HelperSingleVarNoClear),
            "helper-single-const" => Some(Self::HelperSingleConst),
            "entry-single-const" => Some(Self::EntrypointSingleConst),
            _ => None,
        }
    }

    fn body(self) -> &'static str {
        match self {
            Self::EmptyIf => {
                r#"
        if (i >= 0) {
        }
"#
            }
            Self::BoardIf => {
                r#"
        if (OpBin2Num(board[i]) == 0) {
        }
"#
            }
            Self::CounterOnly => {
                r#"
        zero_count = zero_count + 1;
"#
            }
            Self::CounterUnderIf => {
                r#"
        if (OpBin2Num(board[i]) == 0) {
            zero_count = zero_count + 1;
        }
"#
            }
            Self::TwoCountersUnderIf => {
                r#"
        if (OpBin2Num(board[i]) == 0) {
            zero_count = zero_count + 1;
        } else {
            one_count = one_count + 1;
        }
"#
            }
            Self::IfElseTwoUpdates => {
                r#"
        if ((i % 2) == 0) {
            sum = sum + i;
            last = i;
        } else {
            sum = sum - i;
            last = -i;
        }
"#
            }
            Self::NestedIfUpdate => {
                r#"
        if (OpBin2Num(board[i]) == 0) {
            if ((i % 3) == 0) {
                zero_count = zero_count + 2;
            } else {
                zero_count = zero_count + 1;
            }
        }
"#
            }
            Self::CounterPlusMathUnderIf => {
                r#"
        if (OpBin2Num(board[i]) == 0) {
            zero_count = zero_count + 1;
            acc = acc + (i * i) - i;
        } else {
            acc = acc + i;
        }
"#
            }
            Self::RequireInLoop => {
                r#"
        // bounded checks that still read both args and locals
        require(i >= 0);
        require(OpBin2Num(board[i]) >= 0);
        acc = acc + i;
"#
            }
            Self::GuardedPairAdvance => {
                r#"
        if (clear == 1) {
            x = x + step_x;
            y = y + step_y;
        }
"#
            }
            Self::GuardedPairAdvanceWithBool => {
                r#"
        bool at_target = x == target_x && y == target_y;
        if (clear == 1 && !at_target) {
            x = x + step_x;
            y = y + step_y;
        }
"#
            }
            Self::GuardedPairAdvanceWithIndex => {
                r#"
        bool at_target = x == target_x && y == target_y;
        if (clear == 1 && !at_target) {
            int idx = y * 8 + x;
            require(idx >= 0);
            x = x + step_x;
            y = y + step_y;
        }
"#
            }
            Self::GuardedPairAdvanceWithBoardCheck => {
                r#"
        bool at_target = x == target_x && y == target_y;
        if (clear == 1 && !at_target) {
            int idx = y * 8 + x;
            if (OpBin2Num(board[idx]) != 0) {
                clear = 0;
            }
            x = x + step_x;
            y = y + step_y;
        }
"#
            }
            Self::RookHelperExact
            | Self::RookHelperNoBoardCheck
            | Self::HelperSingleVar
            | Self::HelperSingleVarInlineCond
            | Self::HelperSingleVarNoClear
            | Self::HelperSingleConst
            | Self::EntrypointSingleConst => "",
        }
    }

    fn uses_counter(self) -> bool {
        matches!(
            self,
            Self::CounterOnly | Self::CounterUnderIf | Self::TwoCountersUnderIf | Self::NestedIfUpdate | Self::CounterPlusMathUnderIf
        )
    }

    fn name(self) -> &'static str {
        match self {
            Self::EmptyIf => "empty-if",
            Self::BoardIf => "board-if",
            Self::CounterOnly => "counter-only",
            Self::CounterUnderIf => "counter-if",
            Self::TwoCountersUnderIf => "two-counters-if",
            Self::IfElseTwoUpdates => "if-else-two",
            Self::NestedIfUpdate => "nested-if",
            Self::CounterPlusMathUnderIf => "counter-math-if",
            Self::RequireInLoop => "require-in-loop",
            Self::GuardedPairAdvance => "guarded-pair",
            Self::GuardedPairAdvanceWithBool => "guarded-pair-bool",
            Self::GuardedPairAdvanceWithIndex => "guarded-pair-index",
            Self::GuardedPairAdvanceWithBoardCheck => "guarded-pair-board",
            Self::RookHelperExact => "rook-helper",
            Self::RookHelperNoBoardCheck => "rook-helper-noboard",
            Self::HelperSingleVar => "helper-single",
            Self::HelperSingleVarInlineCond => "helper-single-inline",
            Self::HelperSingleVarNoClear => "helper-single-noclear",
            Self::HelperSingleConst => "helper-single-const",
            Self::EntrypointSingleConst => "entry-single-const",
        }
    }

    fn constructor_args(self) -> Vec<Expr<'static>> {
        if matches!(
            self,
            Self::HelperSingleVar
                | Self::HelperSingleVarInlineCond
                | Self::HelperSingleVarNoClear
                | Self::HelperSingleConst
                | Self::EntrypointSingleConst
        ) {
            Vec::new()
        } else {
            vec![Expr::bytes(vec![0u8; 64])]
        }
    }
}

fn source_for_loop_bound(bound: usize, mode: ProbeMode) -> String {
    if matches!(
        mode,
        ProbeMode::RookHelperExact
            | ProbeMode::RookHelperNoBoardCheck
            | ProbeMode::HelperSingleVar
            | ProbeMode::HelperSingleVarInlineCond
            | ProbeMode::HelperSingleVarNoClear
            | ProbeMode::HelperSingleConst
            | ProbeMode::EntrypointSingleConst
    ) {
        if matches!(mode, ProbeMode::HelperSingleVar | ProbeMode::HelperSingleVarInlineCond | ProbeMode::HelperSingleVarNoClear) {
            let guard = if matches!(mode, ProbeMode::HelperSingleVar) {
                r#"
            bool at_target = x == to_x;
            if (clear == 1 && !at_target) {
                x = x + step_x;
            }
"#
            } else if matches!(mode, ProbeMode::HelperSingleVarInlineCond) {
                r#"
            if (clear == 1 && !(x == to_x)) {
                x = x + step_x;
            }
"#
            } else {
                r#"
            if (!(x == to_x)) {
                x = x + step_x;
            }
"#
            };
            return format!(
                r#"
pragma silverscript ^0.1.0;

contract Sweep() {{
    function walk(int from_x, int to_x) : (int) {{
        int step_x = 1;
        int x = from_x + step_x;
        int clear = 1;

        for (i, 0, {bound}, {bound}) {{
{guard}        }}

        return(x);
    }}

    entrypoint function main() {{
        (int x) = walk(0, 7);
        require(x >= 0);
    }}
}}
"#
            );
        }
        if matches!(mode, ProbeMode::HelperSingleConst) {
            return format!(
                r#"
pragma silverscript ^0.1.0;

contract Sweep() {{
    function walk() : (int) {{
        int step_x = 1;
        int x = 1;

        for (i, 0, {bound}, {bound}) {{
            if (!(x == 7)) {{
                x = x + step_x;
            }}
        }}

        return(x);
    }}

    entrypoint function main() {{
        (int x) = walk();
        require(x >= 0);
    }}
}}
"#
            );
        }
        if matches!(mode, ProbeMode::EntrypointSingleConst) {
            return format!(
                r#"
pragma silverscript ^0.1.0;

contract Sweep() {{
    entrypoint function main() {{
        int step_x = 1;
        int x = 1;

        for (i, 0, {bound}, {bound}) {{
            if (!(x == 7)) {{
                x = x + step_x;
            }}
        }}

        require(x >= 0);
    }}
}}
"#
            );
        }

        let board_check = if matches!(mode, ProbeMode::RookHelperExact) {
            r#"
                if (OpBin2Num(board_data[idx]) != 0) {
                    clear = 0;
                }
"#
        } else {
            ""
        };
        return format!(
            r#"
pragma silverscript ^0.1.0;

contract Sweep(byte[64] init_board) {{
    byte[64] board = init_board;

    function rook_path_clear(
        byte[64] board_data,
        int from_x,
        int from_y,
        int to_x,
        int to_y
    ) : (int) {{
        int step_x = 0;
        if (to_x > from_x) {{
            step_x = 1;
        }} else if (to_x < from_x) {{
            step_x = -1;
        }}

        int step_y = 0;
        if (to_y > from_y) {{
            step_y = 1;
        }} else if (to_y < from_y) {{
            step_y = -1;
        }}

        int x = from_x + step_x;
        int y = from_y + step_y;
        int clear = 1;

        for (i, 0, {bound}, {bound}) {{
            bool at_target = x == to_x && y == to_y;
            if (clear == 1 && !at_target) {{
                int idx = y * 8 + x;
{board_check}                x = x + step_x;
                y = y + step_y;
            }}
        }}

        return(clear);
    }}

    entrypoint function main() {{
        byte[64] board_data = board;
        (int clear) = rook_path_clear(board_data, 0, 0, 0, 7);
        require(clear == 0 || clear == 1);
    }}
}}
"#
        );
    }

    let mut decls = String::new();
    if mode.uses_counter() {
        decls.push_str("        int zero_count = 0;\n");
    }
    if matches!(mode, ProbeMode::TwoCountersUnderIf) {
        decls.push_str("        int one_count = 0;\n");
    }
    if matches!(mode, ProbeMode::IfElseTwoUpdates) {
        decls.push_str("        int sum = 0;\n");
        decls.push_str("        int last = 0;\n");
    }
    if matches!(mode, ProbeMode::CounterPlusMathUnderIf | ProbeMode::RequireInLoop) {
        decls.push_str("        int acc = 0;\n");
    }
    if matches!(
        mode,
        ProbeMode::GuardedPairAdvance
            | ProbeMode::GuardedPairAdvanceWithBool
            | ProbeMode::GuardedPairAdvanceWithIndex
            | ProbeMode::GuardedPairAdvanceWithBoardCheck
    ) {
        decls.push_str("        int step_x = 0;\n");
        decls.push_str("        int step_y = 1;\n");
        decls.push_str("        int target_x = 0;\n");
        decls.push_str("        int target_y = 7;\n");
        decls.push_str("        int x = 0;\n");
        decls.push_str("        int y = 1;\n");
        decls.push_str("        int clear = 1;\n");
    }

    let final_check = if matches!(mode, ProbeMode::TwoCountersUnderIf) {
        "        require(zero_count + one_count >= 0);\n"
    } else if matches!(mode, ProbeMode::IfElseTwoUpdates) {
        "        require(sum + last >= -1000000);\n"
    } else if matches!(mode, ProbeMode::CounterPlusMathUnderIf | ProbeMode::RequireInLoop) {
        "        require(acc >= -1000000);\n"
    } else if matches!(
        mode,
        ProbeMode::GuardedPairAdvance
            | ProbeMode::GuardedPairAdvanceWithBool
            | ProbeMode::GuardedPairAdvanceWithIndex
            | ProbeMode::GuardedPairAdvanceWithBoardCheck
    ) {
        "        require(x >= 0 && y >= 0 && clear >= 0);\n"
    } else if mode.uses_counter() {
        "        require(zero_count >= 0);\n"
    } else {
        "        require(true);\n"
    };
    format!(
        r#"
pragma silverscript ^0.1.0;

contract Sweep(byte[64] init_board) {{
    byte[64] board = init_board;

    entrypoint function main() {{
{decls}
        for (i, 0, {bound}, {bound}) {{
{body}
        }}
{final_check}
    }}
}}
"#,
        body = mode.body(),
        decls = decls,
    )
}

fn main() {
    let mut mode = ProbeMode::CounterUnderIf;
    let mut bounds = Vec::new();
    for arg in env::args().skip(1) {
        if let Some(parsed_mode) = ProbeMode::parse(&arg) {
            mode = parsed_mode;
        } else {
            bounds.push(arg.parse().expect("bounds must be integers"));
        }
    }
    let bounds = if bounds.is_empty() { vec![1, 2, 4, 8, 16, 24, 32, 40, 48, 56, 64] } else { bounds };

    for bound in bounds {
        let source = source_for_loop_bound(bound, mode);
        let args = mode.constructor_args();
        let started = Instant::now();
        match compile_contract(&source, &args, CompileOptions::default()) {
            Ok(compiled) => {
                let elapsed_ms = started.elapsed().as_millis();
                let per = (compiled.script.len() as f64) / (bound as f64);
                println!(
                    "mode={} loop_bound={bound} script_len={} bytes_per_iter={:.1} compile_ms={elapsed_ms}",
                    mode.name(),
                    compiled.script.len(),
                    per
                );
            }
            Err(err) => {
                let elapsed_ms = started.elapsed().as_millis();
                println!("mode={} loop_bound={bound} compile_error={err} compile_ms={elapsed_ms}", mode.name());
            }
        }
    }
}
