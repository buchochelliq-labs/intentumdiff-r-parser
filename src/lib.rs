//! R parser plugin — full-parse mode.
//!
//! Handles `.r`, `.R`, `.Rmd` files.
//! Uses tree-sitter-r directly (no Python grammar package needed).
//!
//! Named functions (`foo <- function(x) {...}`) are assignment nodes labelled
//! with the lhs identifier.  `is_method_like_ts` detects function assignments.

use intentumdiff_plugin_sdk::ts_convert::{convert_ts_direct, TsDirectHooks};
use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct RParser;

const TRIVIA: &[&str] = &["comment"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "program",
    // Assignments (named bindings, may define functions)
    "left_assignment",
    "right_assignment",
    "super_assignment",
    "right_super_assignment",
    "equals_assignment",
    // Function definition (anonymous or as rhs of assignment)
    "function_definition",
    "lambda_function",
    // Calls
    "call",
    // Control flow
    "if",
    "for",
    "while",
    "repeat",
    // Blocks / structure
    "block",
    "braced_expression",
    "parenthesized_expression",
    // Operators
    "binary_operator",
    "unary_operator",
    "pipe",
    "special",
    // Namespace / subsetting
    "namespace_operator",
    "dollar",
    "at",
    "subset",
    "subset2",
    // Literals
    "string",
    "integer",
    "float",
    "complex",
    "null",
    "na",
    "na_character_",
    "na_complex_",
    "na_integer_",
    "na_real_",
    "inf",
    "nan",
    "true",
    "false",
    // Names
    "identifier",
    "formal",
    "parameter",
    "argument",
    "return_statement",
    "break",
    "next",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

/// True if this node is an assignment that creates a named function.
fn is_named_function_assignment_ts(node: tree_sitter::Node<'_>) -> bool {
    matches!(
        node.kind(),
        "left_assignment"
            | "right_assignment"
            | "super_assignment"
            | "right_super_assignment"
            | "equals_assignment"
    ) && (0..node.child_count()).any(|i| {
        node.child(i).map_or(false, |c| {
            c.kind() == "function_definition" || c.kind() == "lambda_function"
        })
    })
}

fn is_class_like(_node_type: &str) -> bool {
    false
}

fn is_method_like_ts(node: tree_sitter::Node<'_>) -> bool {
    matches!(node.kind(), "function_definition" | "lambda_function")
        || is_named_function_assignment_ts(node)
}

fn label_for_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let kind = node.kind();
    let txt = |n: tree_sitter::Node<'_>| n.utf8_text(source).unwrap_or("").to_string();
    if node.child_count() == 0 {
        return node.utf8_text(source).unwrap_or("").to_string();
    }
    match kind {
        "left_assignment" | "super_assignment" => {
            for i in 0..node.child_count() {
                let c = node.child(i).unwrap();
                if c.kind() == "identifier" {
                    return txt(c);
                }
            }
        }
        "right_assignment" | "right_super_assignment" => {
            for i in (0..node.child_count()).rev() {
                let c = node.child(i).unwrap();
                if c.kind() == "identifier" {
                    return txt(c);
                }
            }
        }
        "equals_assignment" => {
            for i in 0..node.child_count() {
                let c = node.child(i).unwrap();
                if c.kind() == "identifier" {
                    return txt(c);
                }
            }
        }
        "function_definition" | "lambda_function" => {
            // A function bound via assignment IS named — hoist the binding. Note the
            // modern tree-sitter-r grammar wraps `name <- function(...)` in a
            // binary_operator node (the old *_assignment kinds never match — kind drift,
            // issue #46): take the parent assignment's identifier child. The '(function)'
            // fallback made every R function anonymous, so renames surfaced as add/delete.
            if let Some(parent) = node.parent() {
                let parent_kind = parent.kind();
                if parent_kind == "binary_operator"
                    || is_named_function_assignment_ts(parent)
                {
                    for i in 0..parent.child_count() {
                        let c = parent.child(i).unwrap();
                        if c.kind() == "identifier" {
                            return txt(c);
                        }
                    }
                }
            }
            return "(function)".to_string();
        }
        "call" => {
            if let Some(first) = node.child(0) {
                return txt(first);
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        let c = node.child(i).unwrap();
        if c.kind() == "identifier" {
            return txt(c);
        }
    }
    kind.to_string()
}

fn convert_ts(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    id_prefix: &str,
    parent_class: Option<&str>,
) -> Option<SemanticNode> {
    convert_ts_direct(
        node,
        source,
        id_prefix,
        parent_class,
        &TsDirectHooks {
            is_trivia: &|kind| TRIVIA.contains(&kind),
            class_label: &|_, _| None,
            keep_childless: &|n| is_semantic(n.kind()),
            unwrap_single: &|_, _| false,
            label: &|n, s| label_for_ts(n, s),
            is_method_like: &|n| is_method_like_ts(n),
        },
    )
}

fn process_impl(source: &str) -> String {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_r::LANGUAGE.into();
    if parser.set_language(&lang).is_err() {
        return r#"{"error":"Failed to load R grammar"}"#.to_string();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return r#"{"error":"Parse failed"}"#.to_string(),
    };
    let root = tree.root_node();
    match convert_ts(root, source.as_bytes(), "0", None) {
        Some(n) => serde_json::to_string(&n).unwrap_or_else(|e| format!(r#"{{"error":"{}"}}"#, e)),
        None => r#"{"error":"Empty semantic tree"}"#.to_string(),
    }
}
impl Guest for RParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "r".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".r") || lower.ends_with(".rmd") {
            return "r".to_string();
        }
        // Preserve case for .R (capital R is conventional on Unix)
        if filename.ends_with(".R") {
            return "r".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["r".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "greet <- function(name) {\n  cat(\"Hello, \", name, \"\\n\")\n}\n\nadd <- function(a, b) {\n  a + b\n}\n".to_string(),
            new: "greet <- function(name) {\n  cat(paste0(\"Hello, \", name, \"!\\n\"))\n}\n\nadd <- function(x, y) x + y\n\nmultiply <- function(x, y) x * y\n".to_string(),
        }
    }
}
export!(RParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!RParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = RParser::grammar_id();
        let ids = RParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = RParser::detect_language("test.r".to_string(), "".to_string());
        assert_eq!(r.as_str(), "r");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r = RParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
