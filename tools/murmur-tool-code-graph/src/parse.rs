//! Rust source parsing via `tree-sitter-rust` into symbols + edges.
//!
//! MVP language scope is Rust only. The output structs carry an explicit
//! `language` field ("rust") so the storage layer's `language` columns are
//! populated for real, keeping additional grammars additive later.

use tree_sitter::{Node, Parser};

/// A parsed symbol, ready to insert into the `symbols` table.
pub struct RawSymbol {
    pub symbol_id: String,
    pub language: String,
    pub package: String,
    pub module: String,
    pub qualified_name: String,
    pub simple_name: String,
    pub signature: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub doc_comment: String,
    /// Visibility modifier text exactly as written: `pub`, `pub(crate)`,
    /// `pub(super)`, `pub(in ...)`, or `""` for private/inherited. Only bare
    /// `pub` counts as a public API in classification.
    pub visibility: String,
    /// Raw `#[...]` outer-attribute text preceding the item, newline-joined
    /// (`""` when the item carries no attributes). Used by the classification
    /// heuristics (test/route detection) in `impact_analysis`.
    pub attributes: String,
}

/// A parsed edge. `dst_symbol_id` is `Some` only when the target is known at
/// parse time (structural `contains` edges); `calls` edges are resolved by name
/// later in [`crate::db::resolve_edges`].
pub struct RawEdge {
    pub src_symbol_id: String,
    pub dst_symbol_id: Option<String>,
    pub dst_name: String,
    pub edge_kind: String,
    /// Call syntax for `calls` edges — `"free"` (`foo()`), `"path"`
    /// (`a::b::foo()`), or `"method"` (`x.foo()`); `""` for `contains` edges.
    /// Drives confidence scoring in [`crate::db::resolve_edges`].
    pub call_style: String,
}

pub struct Parsed {
    pub symbols: Vec<RawSymbol>,
    pub edges: Vec<RawEdge>,
}

/// Build the stable symbol identity.
///
/// Format: `rust://<package>/<module>#<qualified_name>(<signature-body>)`
/// where `<signature-body>` is empty for non-functions. Two worked examples are
/// in the build summary. The identity deliberately omits `file:line`, so it
/// survives unrelated edits elsewhere; it deliberately includes the normalized
/// signature, so a change to *this* symbol's own signature yields a *new* id.
pub fn make_symbol_id(package: &str, module: &str, qualified_name: &str, signature_body: &str) -> String {
    format!("rust://{package}/{module}#{qualified_name}({signature_body})")
}

/// Parse one Rust source file. `package` and `module` locate the file within
/// the crate graph (computed by the caller from the filesystem layout).
pub fn parse_file(source: &str, package: &str, module: &str) -> Parsed {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::language())
        .expect("tree-sitter-rust grammar loads");

    let mut out = Parsed { symbols: Vec::new(), edges: Vec::new() };

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return out,
    };
    let bytes = source.as_bytes();
    let root = tree.root_node();

    let mut ctx = Ctx { bytes, package, out: &mut out };
    ctx.walk_items(root, module, None, None);
    out
}

struct Ctx<'a> {
    bytes: &'a [u8],
    package: &'a str,
    out: &'a mut Parsed,
}

impl<'a> Ctx<'a> {
    fn text(&self, node: Node) -> String {
        node.utf8_text(self.bytes).unwrap_or("").to_string()
    }

    /// Collapse internal whitespace runs to a single space and trim — used to
    /// normalize signatures and types so formatting-only edits don't churn ids.
    fn norm(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Walk the direct item children of `node`, emitting symbols and edges.
    ///
    /// `module` is the current module path ("" at crate root, extended by inline
    /// `mod` blocks). `type_prefix` is the impl/trait type used to qualify
    /// method names (`Foo::bar`). `parent` is the nearest enclosing *named*
    /// symbol (mod or trait) for `contains` edges — impl blocks are transparent
    /// and pass their received `parent` through unchanged.
    fn walk_items(
        &mut self,
        node: Node,
        module: &str,
        type_prefix: Option<&str>,
        parent: Option<&str>,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "function_item" | "function_signature_item" => {
                    self.emit_function(child, module, type_prefix, parent);
                }
                "struct_item" | "enum_item" | "union_item" => {
                    self.emit_named(child, module, type_prefix, parent, kind_of(child.kind()));
                }
                "const_item" | "static_item" | "type_item" => {
                    self.emit_named(child, module, type_prefix, parent, kind_of(child.kind()));
                }
                "macro_definition" => {
                    self.emit_named(child, module, type_prefix, parent, "macro");
                }
                "trait_item" => {
                    let sym = self.emit_named(child, module, type_prefix, parent, "trait");
                    if let (Some(name), Some(body)) = (self.field_text(child, "name"), child.child_by_field_name("body")) {
                        // Trait methods qualify under the trait name and are
                        // `contains`-children of the trait symbol.
                        self.walk_items(body, module, Some(&name), sym.as_deref());
                    }
                }
                "mod_item" => {
                    let sym = self.emit_named(child, module, type_prefix, parent, "mod");
                    if let (Some(name), Some(body)) = (self.field_text(child, "name"), child.child_by_field_name("body")) {
                        let inner = if module.is_empty() { name.clone() } else { format!("{module}::{name}") };
                        self.walk_items(body, &inner, None, sym.as_deref());
                    }
                }
                "impl_item" => {
                    // Impl blocks are transparent: they contribute a type prefix
                    // to their methods but are not themselves addressable symbols.
                    let ty = child
                        .child_by_field_name("type")
                        .map(|n| strip_generics(&self.text(n)))
                        .unwrap_or_default();
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk_items(body, module, Some(&ty), parent);
                    }
                }
                _ => {}
            }
        }
    }

    fn field_text(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field).map(|n| self.text(n))
    }

    /// Emit a symbol for a simple named item (struct/enum/const/...). Returns
    /// the new symbol_id so callers can thread it as a `contains` parent.
    fn emit_named(
        &mut self,
        node: Node,
        module: &str,
        type_prefix: Option<&str>,
        parent: Option<&str>,
        kind: &str,
    ) -> Option<String> {
        let name = self.field_text(node, "name")?;
        let qualified = qualify(type_prefix, &name);
        let symbol_id = make_symbol_id(self.package, module, &qualified, "");
        self.push_symbol(&symbol_id, module, &qualified, "", kind, node);
        self.push_contains(parent, &symbol_id, &name);
        Some(symbol_id)
    }

    fn emit_function(
        &mut self,
        node: Node,
        module: &str,
        type_prefix: Option<&str>,
        parent: Option<&str>,
    ) {
        let Some(name) = self.field_text(node, "name") else { return };
        let qualified = qualify(type_prefix, &name);
        let signature = self.signature_body(node);
        let symbol_id = make_symbol_id(self.package, module, &qualified, &signature);
        self.push_symbol(&symbol_id, module, &qualified, &signature, "function", node);
        self.push_contains(parent, &symbol_id, &name);
        // Collect callees from the body (function_signature_item has none).
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, &symbol_id);
        }
    }

    /// Build the normalized signature body: comma-joined parameter types plus an
    /// optional `->ret`. Parameter *names* are excluded so renaming a binding
    /// does not mint a new id, while a type change does.
    fn signature_body(&self, func: Node) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(params) = func.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for p in params.named_children(&mut cursor) {
                match p.kind() {
                    "self_parameter" => parts.push(Self::norm(&self.text(p))),
                    "parameter" => {
                        let ty = p
                            .child_by_field_name("type")
                            .map(|n| Self::norm(&self.text(n)))
                            .unwrap_or_else(|| "_".to_string());
                        parts.push(ty);
                    }
                    "variadic_parameter" => parts.push("...".to_string()),
                    _ => {}
                }
            }
        }
        let mut sig = parts.join(",");
        if let Some(ret) = func.child_by_field_name("return_type") {
            sig.push_str("->");
            sig.push_str(&Self::norm(&self.text(ret)));
        }
        sig
    }

    fn push_symbol(
        &mut self,
        symbol_id: &str,
        module: &str,
        qualified: &str,
        signature: &str,
        kind: &str,
        node: Node,
    ) {
        // The simple name is the final `::` segment of the qualified name.
        let simple = qualified.rsplit("::").next().unwrap_or(qualified);
        self.out.symbols.push(RawSymbol {
            symbol_id: symbol_id.to_string(),
            language: "rust".to_string(),
            package: self.package.to_string(),
            module: module.to_string(),
            qualified_name: qualified.to_string(),
            simple_name: simple.to_string(),
            signature: signature.to_string(),
            kind: kind.to_string(),
            start_line: node.start_position().row as i64 + 1,
            end_line: node.end_position().row as i64 + 1,
            doc_comment: self.doc_comment(node),
            visibility: self.visibility(node),
            attributes: self.attributes(node),
        });
    }

    fn push_contains(&mut self, parent: Option<&str>, child_id: &str, child_name: &str) {
        if let Some(p) = parent {
            self.out.edges.push(RawEdge {
                src_symbol_id: p.to_string(),
                dst_symbol_id: Some(child_id.to_string()),
                dst_name: child_name.to_string(),
                edge_kind: "contains".to_string(),
                call_style: String::new(),
            });
        }
    }

    /// The item's visibility modifier as written, or `""` when private.
    ///
    /// tree-sitter-rust 0.21 does not expose `visibility` as a named field on
    /// item nodes, so we scan the item's direct children for a
    /// `visibility_modifier` node instead. Its text is `pub`, `pub(crate)`,
    /// `pub(super)`, or `pub(in <path>)`; only bare `pub` is a public API.
    fn visibility(&self, node: Node) -> String {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "visibility_modifier" {
                return Self::norm(&self.text(child));
            }
        }
        String::new()
    }

    /// Gather contiguous preceding `#[...]` outer attributes, newline-joined in
    /// source order. Interspersed doc/line comments are skipped (they are the
    /// same structural position); any other preceding sibling stops the walk.
    /// Analogous to [`Ctx::doc_comment`], but collecting `attribute_item`s.
    fn attributes(&self, node: Node) -> String {
        let mut items: Vec<String> = Vec::new();
        let mut sib = node.prev_sibling();
        while let Some(s) = sib {
            match s.kind() {
                "attribute_item" => items.push(self.text(s).trim().to_string()),
                // Doc/line/block comments may sit between attributes and the item.
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            sib = s.prev_sibling();
        }
        items.reverse();
        items.join("\n")
    }

    /// Walk a function body subtree collecting the simple names of call targets
    /// along with the syntax each was written in. Handles free calls (`foo()`),
    /// path calls (`a::b::foo()`), and method calls (`x.foo()`); the last
    /// `::`/field segment is the recorded name. Names are deduped per source
    /// symbol; if the same name is called both ways, `method` (the weakest /
    /// least type-certain style) wins so confidence stays conservative.
    fn collect_calls(&mut self, node: Node, src: &str) {
        let mut seen: Vec<(String, String)> = Vec::new();
        self.walk_calls(node, &mut seen);
        for (name, style) in seen {
            self.out.edges.push(RawEdge {
                src_symbol_id: src.to_string(),
                dst_symbol_id: None,
                dst_name: name,
                edge_kind: "calls".to_string(),
                call_style: style,
            });
        }
    }

    fn walk_calls(&self, node: Node, seen: &mut Vec<(String, String)>) {
        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                if let Some((name, style)) = self.callee_name(func) {
                    match seen.iter_mut().find(|(n, _)| *n == name) {
                        Some(existing) if style == "method" => existing.1 = "method".to_string(),
                        Some(_) => {}
                        None => seen.push((name, style)),
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk_calls(child, seen);
        }
    }

    /// Resolve a call's callee to `(simple_name, call_style)`.
    fn callee_name(&self, func: Node) -> Option<(String, String)> {
        match func.kind() {
            "identifier" => Some((self.text(func), "free".to_string())),
            // a::b::foo -> foo ; the `name` field is the final segment.
            "scoped_identifier" => {
                let name = func
                    .child_by_field_name("name")
                    .map(|n| self.text(n))
                    .unwrap_or_else(|| self.text(func));
                Some((name, "path".to_string()))
            }
            // x.foo() / x.foo::<T>() -> foo (receiver type unknown at Level 1).
            "field_expression" => {
                func.child_by_field_name("field").map(|n| (self.text(n), "method".to_string()))
            }
            "generic_function" => func.child_by_field_name("function").and_then(|f| self.callee_name(f)),
            _ => None,
        }
    }

    /// Gather contiguous preceding `///` / `//!` / `/** */` doc comments.
    fn doc_comment(&self, node: Node) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut sib = node.prev_sibling();
        while let Some(s) = sib {
            let kind = s.kind();
            if kind != "line_comment" && kind != "block_comment" {
                break;
            }
            let raw = self.text(s);
            let t = raw.trim();
            let is_doc = t.starts_with("///") || t.starts_with("//!") || t.starts_with("/**") || t.starts_with("/*!");
            if !is_doc {
                break;
            }
            lines.push(clean_doc_line(t));
            sib = s.prev_sibling();
        }
        lines.reverse();
        lines.join("\n").trim().to_string()
    }
}

fn clean_doc_line(t: &str) -> String {
    let t = t
        .trim_start_matches("///")
        .trim_start_matches("//!")
        .trim_start_matches("/**")
        .trim_start_matches("/*!")
        .trim_end_matches("*/");
    t.trim().to_string()
}

fn qualify(type_prefix: Option<&str>, name: &str) -> String {
    match type_prefix {
        Some(t) if !t.is_empty() => format!("{t}::{name}"),
        _ => name.to_string(),
    }
}

fn strip_generics(ty: &str) -> String {
    let ty = ty.trim();
    match ty.find('<') {
        Some(i) => ty[..i].trim().to_string(),
        None => ty.to_string(),
    }
}

fn kind_of(node_kind: &str) -> &'static str {
    match node_kind {
        "struct_item" => "struct",
        "enum_item" => "enum",
        "union_item" => "union",
        "const_item" => "const",
        "static_item" => "static",
        "type_item" => "type",
        _ => "item",
    }
}
