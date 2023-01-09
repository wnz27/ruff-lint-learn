use libcst_native::{parse_expression, Arg, Codegen, CodegenState, Expression};
use num_bigint::{BigInt, Sign};
use once_cell::sync::Lazy;
use regex::Regex;
use rustpython_ast::{Constant, Expr, ExprKind};
use rustpython_parser::lexer;
use rustpython_parser::lexer::Tok;

use crate::ast::types::Range;
use crate::autofix::Fix;
use crate::checkers::ast::Checker;
use crate::cst::matchers::{match_call, match_expression};
use crate::registry::Diagnostic;
use crate::violations;

// The regex documentation says to do this because creating regexs is expensive:
// https://docs.rs/regex/latest/regex/#example-avoid-compiling-the-same-regex-in-a-loop
static FORMAT_SPECIFIER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{(?P<int>\d+)(?P<fmt>.*?)\}").unwrap());

/// Convert a python integer to a unsigned 32 but integer. We are assuming this
/// will never overflow because people will probably never have more than 2^32
/// arguments to a format string. I am also ignoring the signed, I personally
/// checked and negative numbers are not allowed in format strings
fn convert_big_int(bigint: BigInt) -> Option<u32> {
    let (sign, digits) = bigint.to_u32_digits();
    match sign {
        Sign::Plus => digits.get(0).copied(),
        Sign::Minus => None,
        Sign::NoSign => Some(0),
    }
}

fn get_new_args(old_args: Vec<Arg>, correct_order: Vec<u32>) -> Vec<Arg> {
    let mut new_args: Vec<Arg> = Vec::new();
    for (i, given_idx) in correct_order.iter().enumerate() {
        // We need to keep the formatting in the same order but move the values
        let values = old_args.get(given_idx.to_owned() as usize).unwrap();
        let formatting = old_args.get(i).unwrap();
        let new_arg = Arg {
            value: values.value.clone(),
            comma: formatting.comma.clone(),
            // Kwargs are NOT allowed in .format (I checked)
            equal: None,
            keyword: None,
            star: values.star,
            whitespace_after_star: formatting.whitespace_after_star.clone(),
            whitespace_after_arg: formatting.whitespace_after_arg.clone(),
        };
        new_args.push(new_arg);
    }
    new_args
}

/// Returns the new call string, or returns an error if it cannot create a new call string
fn get_new_call(module_text: &str, correct_order: Vec<u32>) -> Result<String, ()> {
    let mut expression = match parse_expression(&module_text) {
        Err(_) => return Err(()),
        Ok(item) => item,
    };
    let mut call = match match_call(&mut expression) {
        Err(_) => return Err(()),
        Ok(item) => item,
    };
    call.args = get_new_args(call.args.clone(), correct_order);
    // Create the new function
    if let Expression::Attribute(item) = &*call.func {
        // Converting the struct to a struct and then back is not very efficient, but
        // regexs were the simplest way I could find to remove the specifiers
        let mut state = CodegenState::default();
        item.codegen(&mut state);
        let cleaned = remove_specifiers(&state.to_string());
        match match_expression(&cleaned) {
            Err(_) => return Err(()),
            Ok(item) => call.func = Box::new(item),
        };
        // Create the string
        let mut final_state = CodegenState::default();
        expression.codegen(&mut final_state);
        return Ok(final_state.to_string());
    }
    Err(())
}

fn get_specifier_order(value_str: &str) -> Vec<u32> {
    let mut specifier_ints: Vec<u32> = vec![];
    // Whether the previous character was a Lbrace. If this is true and the next
    // character is an integer than this integer gets added to the list of
    // constants
    let mut prev_l_brace = false;
    for (_, tok, _) in lexer::make_tokenizer(value_str).flatten() {
        if Tok::Lbrace == tok {
            prev_l_brace = true;
        } else if let Tok::Int { value } = tok {
            if prev_l_brace {
                if let Some(int_val) = convert_big_int(value) {
                    specifier_ints.push(int_val);
                }
            }
            prev_l_brace = false;
        } else {
            prev_l_brace = false;
        }
    }
    specifier_ints
}

/// Returns a string without the format specifiers. Ex. "Hello {0} {1}" ->
/// "Hello {} {}"
fn remove_specifiers(raw_specifiers: &str) -> String {
    let new_str = FORMAT_SPECIFIER
        .replace_all(raw_specifiers, "{$fmt}")
        .to_string();
    new_str
}

/// Checks if there is a single specifier in the string. The string must either
/// have all formatterts or no formatters (or else an error will be thrown), so
/// this will work as long as the python code is valid
fn has_specifiers(raw_specifiers: &str) -> bool {
    FORMAT_SPECIFIER.is_match(raw_specifiers)
}

/// UP030
pub fn format_specifiers(checker: &mut Checker, expr: &Expr, func: &Expr) {
    if let ExprKind::Attribute { value, attr, .. } = &func.node {
        if let ExprKind::Constant {
            value: cons_value, ..
        } = &value.node
        {
            if let Constant::Str(provided_string) = cons_value {
                // The function must be a format function
                if attr != "format" {
                    return;
                }
                // The squigly brackets must have format specifiers inside of them
                if !has_specifiers(provided_string) {
                    return;
                }
                let as_ints = get_specifier_order(provided_string);
                let call_range = Range::from_located(expr);
                let call_text = checker.locator.slice_source_code_range(&call_range);
                let mut diagnostic =
                    Diagnostic::new(violations::FormatSpecifiers, Range::from_located(expr));
                match get_new_call(&call_text, as_ints) {
                    // If we get any errors, we know that there is an issue that we cannot fix
                    // so we should just report that there is a formatting issue. Currently the
                    // only issue we know of is a ParseError from a multi line format statement
                    // inside a function call that does not explicitly say there are multiple
                    // lines. Follow my Github issue here:
                    // https://github.com/Instagram/LibCST/issues/846

                    // Is there a way to specify that here this is not fixable, but below it is??
                    Err(_) => checker.diagnostics.push(diagnostic),
                    Ok(new_call) => {
                        if checker.patch(diagnostic.kind.code()) {
                            diagnostic.amend(Fix::replacement(
                                new_call,
                                expr.location,
                                expr.end_location.unwrap(),
                            ));
                        }
                        checker.diagnostics.push(diagnostic);
                    }
                };
            }
        }
    }
}