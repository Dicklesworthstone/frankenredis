#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_command::eval_script;
use fr_protocol::RespFrame;
use fr_store::Store;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 4_096;
const MAX_RAW_SCRIPT_LEN: usize = 512;
const MAX_TEXT_LEN: usize = 24;

#[derive(Debug, Arbitrary)]
enum StructuredLuaCase {
    ReturnScalar {
        expr: ScalarExpr,
    },
    LocalAlias {
        name_seed: u8,
        expr: ScalarExpr,
    },
    IfElse {
        condition: ConditionExpr,
        then_expr: ScalarExpr,
        else_expr: ScalarExpr,
    },
    NumericForSum {
        start: i8,
        stop: i8,
        step: i8,
    },
    FunctionIdentity {
        function_seed: u8,
        arg_seed: u8,
        argument: ScalarExpr,
    },
    TableIndex {
        first: ScalarExpr,
        second: ScalarExpr,
        index_seed: u8,
    },
    KeysArgEcho {
        key: Vec<u8>,
        arg: Vec<u8>,
    },
}

#[derive(Debug, Clone, Arbitrary)]
enum ScalarExpr {
    Integer(i16),
    Bool(bool),
    String(Vec<u8>),
    UnaryMinus(i16),
    Not(bool),
    Add(i16, i16),
    Multiply(i16, i16),
    Modulo(i16, i8),
    Concat(Vec<u8>, Vec<u8>),
    LengthOfString(Vec<u8>),
}

#[derive(Debug, Clone, Arbitrary)]
enum ConditionExpr {
    Bool(bool),
    LessThan(i16, i16),
    EqualInt(i16, i16),
    EqualString(Vec<u8>, Vec<u8>),
    Not(bool),
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    let Some((&mode, body)) = data.split_first() else {
        return;
    };

    match mode % 2 {
        0 => fuzz_raw_lua(body),
        _ => {
            let mut unstructured = Unstructured::new(body);
            let Ok(case) = StructuredLuaCase::arbitrary(&mut unstructured) else {
                return;
            };
            fuzz_structured_lua(case);
        }
    }
});

fn fuzz_raw_lua(body: &[u8]) {
    let mut script = body.to_vec();
    script.truncate(MAX_RAW_SCRIPT_LEN);

    let keys = vec![b"key".to_vec()];
    let argv = vec![b"arg".to_vec()];
    let mut store = Store::new();
    let _ = eval_script(&script, &keys, &argv, &mut store, 0);
}

fn fuzz_structured_lua(case: StructuredLuaCase) {
    let rendered = render_case(&case);

    let base_result = run_script(&rendered.base_script, &rendered.keys, &rendered.argv);
    let rerun_result = run_script(&rendered.base_script, &rendered.keys, &rendered.argv);
    let padded_result = run_script(&rendered.padded_script, &rendered.keys, &rendered.argv);

    assert_eq!(
        base_result, rerun_result,
        "pure structured Lua scripts must be deterministic across fresh stores",
    );
    assert_eq!(
        base_result, padded_result,
        "harmless whitespace and semicolon padding must not change pure Lua results",
    );
}

fn run_script(script: &[u8], keys: &[Vec<u8>], argv: &[Vec<u8>]) -> Result<RespFrame, String> {
    let mut store = Store::new();
    eval_script(script, keys, argv, &mut store, 0)
}

struct RenderedCase {
    base_script: Vec<u8>,
    padded_script: Vec<u8>,
    keys: Vec<Vec<u8>>,
    argv: Vec<Vec<u8>>,
}

fn render_case(case: &StructuredLuaCase) -> RenderedCase {
    match case {
        StructuredLuaCase::ReturnScalar { expr } => {
            let expr = render_scalar_expr(expr);
            render_with_padding(format!("return {expr}"))
        }
        StructuredLuaCase::LocalAlias { name_seed, expr } => {
            let name = render_name("value", *name_seed);
            let expr = render_scalar_expr(expr);
            let base = format!("local {name} = {expr}\nreturn {name}");
            let padded = format!("do\n  local {name} = {expr}\n  return {name}\nend");
            RenderedCase {
                base_script: base.into_bytes(),
                padded_script: padded.into_bytes(),
                keys: Vec::new(),
                argv: Vec::new(),
            }
        }
        StructuredLuaCase::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            let condition = render_condition(condition);
            let then_expr = render_scalar_expr(then_expr);
            let else_expr = render_scalar_expr(else_expr);
            let base = format!(
                "if {condition} then\n  return {then_expr}\nelse\n  return {else_expr}\nend"
            );
            let padded = format!(
                "local out\nif {condition} then\n  out = {then_expr}\nelse\n  out = {else_expr}\nend\nreturn out"
            );
            RenderedCase {
                base_script: base.into_bytes(),
                padded_script: padded.into_bytes(),
                keys: Vec::new(),
                argv: Vec::new(),
            }
        }
        StructuredLuaCase::NumericForSum { start, stop, step } => {
            let step = non_zero_step(*step);
            let base = format!(
                "local total = 0\nfor i = {start}, {stop}, {step} do\n  total = total + i\nend\nreturn total"
            );
            let padded = format!(
                "do\n  local total = 0\n  for i = {start}, {stop}, {step} do\n    total = total + i\n  end\n  return total\nend"
            );
            RenderedCase {
                base_script: base.into_bytes(),
                padded_script: padded.into_bytes(),
                keys: Vec::new(),
                argv: Vec::new(),
            }
        }
        StructuredLuaCase::FunctionIdentity {
            function_seed,
            arg_seed,
            argument,
        } => {
            let function_name = render_name("fn", *function_seed);
            let arg_name = render_name("arg", *arg_seed);
            let argument = render_scalar_expr(argument);
            let base = format!(
                "local function {function_name}({arg_name})\n  return {arg_name}\nend\nreturn {function_name}({argument})"
            );
            let padded = format!(
                "local {function_name}\n{function_name} = function({arg_name})\n  return {arg_name}\nend\nreturn {function_name}({argument})"
            );
            RenderedCase {
                base_script: base.into_bytes(),
                padded_script: padded.into_bytes(),
                keys: Vec::new(),
                argv: Vec::new(),
            }
        }
        StructuredLuaCase::TableIndex {
            first,
            second,
            index_seed,
        } => {
            let first = render_scalar_expr(first);
            let second = render_scalar_expr(second);
            let index = usize::from(index_seed % 2) + 1;
            let base = format!("local t = {{ {first}, {second} }}\nreturn t[{index}]");
            let padded =
                format!("local t = {{}}\nt[1] = {first}\nt[2] = {second}\nreturn t[{index}]");
            RenderedCase {
                base_script: base.into_bytes(),
                padded_script: padded.into_bytes(),
                keys: Vec::new(),
                argv: Vec::new(),
            }
        }
        StructuredLuaCase::KeysArgEcho { key, arg } => {
            let key = sanitize_binary_value(key, b"key");
            let arg = sanitize_binary_value(arg, b"arg");
            let base = "return {KEYS[1], ARGV[1]}".to_string();
            let padded = "local key = KEYS[1]\nlocal arg = ARGV[1]\nreturn {key, arg}".to_string();
            RenderedCase {
                base_script: base.into_bytes(),
                padded_script: padded.into_bytes(),
                keys: vec![key],
                argv: vec![arg],
            }
        }
    }
}

fn render_with_padding(script: String) -> RenderedCase {
    // Only add leading whitespace/semicolons - trailing semicolons after
    // `return` are invalid in Lua (return must be last statement in block).
    let padded = format!("\n;;\n{script}\n");
    RenderedCase {
        base_script: script.into_bytes(),
        padded_script: padded.into_bytes(),
        keys: Vec::new(),
        argv: Vec::new(),
    }
}

fn render_scalar_expr(expr: &ScalarExpr) -> String {
    match expr {
        ScalarExpr::Integer(value) => value.to_string(),
        ScalarExpr::Bool(value) => {
            if *value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        ScalarExpr::String(bytes) => quote_lua_string(bytes, "value"),
        ScalarExpr::UnaryMinus(value) => format!("(-{})", i32::from(*value).abs()),
        ScalarExpr::Not(value) => {
            if *value {
                "(not true)".to_string()
            } else {
                "(not false)".to_string()
            }
        }
        ScalarExpr::Add(left, right) => format!("({left} + {right})"),
        ScalarExpr::Multiply(left, right) => format!("({left} * {right})"),
        ScalarExpr::Modulo(left, right) => {
            let right = non_zero_step(*right);
            format!("({left} % {right})")
        }
        ScalarExpr::Concat(left, right) => format!(
            "({} .. {})",
            quote_lua_string(left, "left"),
            quote_lua_string(right, "right")
        ),
        ScalarExpr::LengthOfString(bytes) => format!("#{}", quote_lua_string(bytes, "len")),
    }
}

fn render_condition(condition: &ConditionExpr) -> String {
    match condition {
        ConditionExpr::Bool(value) => {
            if *value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        ConditionExpr::LessThan(left, right) => format!("({left} < {right})"),
        ConditionExpr::EqualInt(left, right) => format!("({left} == {right})"),
        ConditionExpr::EqualString(left, right) => format!(
            "({} == {})",
            quote_lua_string(left, "lhs"),
            quote_lua_string(right, "rhs")
        ),
        ConditionExpr::Not(value) => {
            if *value {
                "(not true)".to_string()
            } else {
                "(not false)".to_string()
            }
        }
    }
}

fn render_name(prefix: &str, seed: u8) -> String {
    let suffix = char::from(b'a' + (seed % 26));
    format!("{prefix}_{suffix}")
}

fn non_zero_step(step: i8) -> i16 {
    let step = i16::from(step);
    if step == 0 { 1 } else { step }
}

fn quote_lua_string(bytes: &[u8], fallback: &str) -> String {
    let content = sanitize_text(bytes, fallback);
    let mut quoted = String::with_capacity(content.len() + 2);
    quoted.push('"');
    for ch in content.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn sanitize_text(bytes: &[u8], fallback: &str) -> String {
    let text: String = bytes
        .iter()
        .filter_map(|byte| {
            let ch = *byte as char;
            (ch.is_ascii_graphic() || ch == ' ').then_some(ch)
        })
        .take(MAX_TEXT_LEN)
        .collect();
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

fn sanitize_binary_value(bytes: &[u8], fallback: &[u8]) -> Vec<u8> {
    let mut value: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|byte| *byte != b'\r' && *byte != b'\n')
        .take(MAX_TEXT_LEN)
        .collect();
    if value.is_empty() {
        value = fallback.to_vec();
    }
    value
}
