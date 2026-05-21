//! Maps file extensions to tree-sitter grammars and gives each grammar a
//! list of node-kinds that count as "symbols" or "calls".

use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Java,
}

impl Lang {
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "ts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "java" => Lang::Java,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Java => "java",
        }
    }

    pub fn ts_language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::language(),
            Lang::TypeScript => tree_sitter_typescript::language_typescript(),
            Lang::Tsx => tree_sitter_typescript::language_tsx(),
            Lang::JavaScript => tree_sitter_javascript::language(),
            Lang::Python => tree_sitter_python::language(),
            Lang::Go => tree_sitter_go::language(),
            Lang::Java => tree_sitter_java::language(),
        }
    }

    /// Tree-sitter node kinds that represent a top-level declaration we
    /// want to record. Returns `(node_kind, symbol_kind)` pairs.
    pub fn symbol_kinds(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Lang::Rust => &[
                ("function_item", "function"),
                ("struct_item", "struct"),
                ("enum_item", "enum"),
                ("impl_item", "impl"),
                ("trait_item", "trait"),
                ("const_item", "const"),
                ("type_item", "type"),
                ("mod_item", "module"),
            ],
            Lang::TypeScript | Lang::Tsx => &[
                ("function_declaration", "function"),
                ("class_declaration", "class"),
                ("method_definition", "method"),
                ("interface_declaration", "interface"),
                ("type_alias_declaration", "type"),
                ("variable_declarator", "const"),
                ("abstract_class_declaration", "class"),
                ("enum_declaration", "enum"),
            ],
            Lang::JavaScript => &[
                ("function_declaration", "function"),
                ("class_declaration", "class"),
                ("method_definition", "method"),
                ("variable_declarator", "const"),
            ],
            Lang::Python => &[
                ("function_definition", "function"),
                ("class_definition", "class"),
            ],
            Lang::Go => &[
                ("function_declaration", "function"),
                ("method_declaration", "method"),
                ("type_spec", "type"),
            ],
            Lang::Java => &[
                ("class_declaration", "class"),
                ("method_declaration", "method"),
                ("interface_declaration", "interface"),
                ("enum_declaration", "enum"),
            ],
        }
    }

    /// Tree-sitter node kinds that represent a call expression.
    pub fn call_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["call_expression", "macro_invocation"],
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => &["call_expression"],
            Lang::Python => &["call"],
            Lang::Go => &["call_expression"],
            Lang::Java => &["method_invocation"],
        }
    }
}
