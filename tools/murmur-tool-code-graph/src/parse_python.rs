//! Python source parsing via `tree-sitter-python` into symbols + edges.
//!
//! The counterpart to [`crate::parse`] (Rust). It emits the same
//! [`crate::parse::RawSymbol`] / [`crate::parse::RawEdge`] / [`crate::parse::Parsed`]
//! shapes so the storage and traversal layers need no changes — only the grammar
//! walk and the identity scheme differ:
//!
//! - Symbol ids use the `python://` scheme (via [`crate::parse::make_symbol_id`]).
//! - `visibility` follows the underscore-prefix convention: `"pub"` for a
//!   module-/class-level name not starting with `_`, `""` otherwise.
//! - `attributes` captures decorator source text (`@app.route(...)`), newline-joined.
//! - Every attribute-qualified call (`x.foo()`) is tagged `call_style = "method"` —
//!   Python's grammar cannot statically distinguish a module-qualified free call
//!   (`os.path.join()`) from a true dynamic dispatch, so all attribute calls are
//!   conservatively `"method"` (→ always `"heuristic"` confidence). Only
//!   bare-identifier calls (`foo()`) are `"free"`. `"path"` is never emitted.

use tree_sitter::{Node, Parser};

use crate::parse::{make_symbol_id, Parsed, RawEdge, RawSymbol};

/// Parse one Python source file. `package` and `module` locate the file within
/// the package graph (computed by the caller from the filesystem layout).
pub fn parse_file(source: &str, package: &str, module: &str) -> Parsed {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::language())
        .expect("tree-sitter-python grammar loads");

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

impl Ctx<'_> {
    fn text(&self, node: Node) -> String {
        node.utf8_text(self.bytes).unwrap_or("").to_string()
    }

    /// Collapse internal whitespace runs to a single space and trim — used to
    /// normalize signatures so formatting-only edits don't churn ids.
    fn norm(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Walk the direct definition children of `node`, emitting symbols and edges.
    ///
    /// `module` is the current dotted module path. `type_prefix` is the enclosing
    /// class name used to qualify method names (`Foo::bar`, `::`-separated to
    /// match the engine's `simple_name` extraction). `parent` is the nearest
    /// enclosing *named* symbol (a class) for `contains` edges.
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
                "function_definition" => {
                    self.emit_function(child, module, type_prefix, parent, "");
                }
                "class_definition" => {
                    self.emit_class(child, module, type_prefix, parent, "");
                }
                // A decorated `def`/`class`: gather the decorator text and hand it
                // to the inner definition as its `attributes`.
                "decorated_definition" => {
                    let attrs = self.decorator_text(child);
                    if let Some(def) = child.child_by_field_name("definition") {
                        match def.kind() {
                            "function_definition" => {
                                self.emit_function(def, module, type_prefix, parent, &attrs);
                            }
                            "class_definition" => {
                                self.emit_class(def, module, type_prefix, parent, &attrs);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn field_text(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field).map(|n| self.text(n))
    }

    /// Emit a `class` symbol and recurse into its body for methods (which qualify
    /// under the class name and are `contains`-children of the class symbol).
    fn emit_class(
        &mut self,
        node: Node,
        module: &str,
        type_prefix: Option<&str>,
        parent: Option<&str>,
        attributes: &str,
    ) {
        let Some(name) = self.field_text(node, "name") else { return };
        let qualified = qualify(type_prefix, &name);
        let symbol_id = make_symbol_id("python", self.package, module, &qualified, "");
        self.push_symbol(&symbol_id, module, &qualified, "", "class", node, attributes);
        self.push_contains(parent, &symbol_id, &name);
        if let Some(body) = node.child_by_field_name("body") {
            self.walk_items(body, module, Some(&qualified), Some(&symbol_id));
        }
    }

    fn emit_function(
        &mut self,
        node: Node,
        module: &str,
        type_prefix: Option<&str>,
        parent: Option<&str>,
        attributes: &str,
    ) {
        let Some(name) = self.field_text(node, "name") else { return };
        let qualified = qualify(type_prefix, &name);
        let signature = self.signature_body(node);
        let symbol_id = make_symbol_id("python", self.package, module, &qualified, &signature);
        self.push_symbol(&symbol_id, module, &qualified, &signature, "function", node, attributes);
        self.push_contains(parent, &symbol_id, &name);
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, &symbol_id);
        }
    }

    /// Build the normalized signature body: the comma-joined normalized text of
    /// each parameter, plus an optional `->ret` return annotation. The parameter
    /// *names* are retained (unlike Rust, where types carry the identity) because
    /// Python parameters are usually unannotated, so names are the only reliable
    /// arity/identity signal. Any change to the declared parameter list or the
    /// return annotation therefore mints a new id; nothing else does.
    fn signature_body(&self, func: Node) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(params) = func.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for p in params.named_children(&mut cursor) {
                // Skip comments that may appear between parameters.
                if p.kind() == "comment" {
                    continue;
                }
                parts.push(Self::norm(&self.text(p)));
            }
        }
        let mut sig = parts.join(",");
        if let Some(ret) = func.child_by_field_name("return_type") {
            sig.push_str("->");
            sig.push_str(&Self::norm(&self.text(ret)));
        }
        sig
    }

    #[allow(clippy::too_many_arguments)]
    fn push_symbol(
        &mut self,
        symbol_id: &str,
        module: &str,
        qualified: &str,
        signature: &str,
        kind: &str,
        node: Node,
        attributes: &str,
    ) {
        // The simple name is the final `::` segment of the qualified name.
        let simple = qualified.rsplit("::").next().unwrap_or(qualified);
        self.out.symbols.push(RawSymbol {
            symbol_id: symbol_id.to_string(),
            language: "python".to_string(),
            package: self.package.to_string(),
            module: module.to_string(),
            qualified_name: qualified.to_string(),
            simple_name: simple.to_string(),
            signature: signature.to_string(),
            kind: kind.to_string(),
            start_line: node.start_position().row as i64 + 1,
            end_line: node.end_position().row as i64 + 1,
            doc_comment: self.doc_comment(node),
            visibility: visibility_of(simple),
            attributes: attributes.to_string(),
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

    /// Newline-join the source text of every `decorator` child of a
    /// `decorated_definition`, in source order (mirrors how the Rust parser
    /// gathers preceding `#[...]` attributes). The `@` is preserved so the
    /// route/marker scan in `impact_analysis` sees `@app.get(...)` verbatim.
    fn decorator_text(&self, decorated: Node) -> String {
        let mut items: Vec<String> = Vec::new();
        let mut cursor = decorated.walk();
        for child in decorated.named_children(&mut cursor) {
            if child.kind() == "decorator" {
                items.push(self.text(child).trim().to_string());
            }
        }
        items.join("\n")
    }

    /// Walk a function body subtree collecting the simple names of call targets
    /// along with the syntax each was written in. A bare `foo()` is `"free"`;
    /// every attribute-qualified call `x.foo()` / `a.b.foo()` is `"method"`
    /// (Python cannot statically tell a module-qualified free call from a dynamic
    /// dispatch). Names are deduped per source symbol; if the same name is called
    /// both ways, `method` (the weaker style) wins so confidence stays conservative.
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
        if node.kind() == "call" {
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
            // x.foo() / a.b.foo() -> foo ; the receiver's type is unknown, so
            // every attribute call is conservatively a method call.
            "attribute" => {
                func.child_by_field_name("attribute").map(|n| (self.text(n), "method".to_string()))
            }
            _ => None,
        }
    }

    /// Extract a symbol's docstring: the string literal of the first statement in
    /// its body, if present. Triple/single quotes and the `r`/`b`/`f` string
    /// prefixes are stripped; interior text is trimmed. `""` when absent.
    fn doc_comment(&self, node: Node) -> String {
        let Some(body) = node.child_by_field_name("body") else { return String::new() };
        let mut cursor = body.walk();
        let Some(first) = body.named_children(&mut cursor).next() else { return String::new() };
        if first.kind() != "expression_statement" {
            return String::new();
        }
        let mut inner = first.walk();
        let Some(expr) = first.named_children(&mut inner).next() else { return String::new() };
        if expr.kind() != "string" {
            return String::new();
        }
        clean_docstring(&self.text(expr))
    }
}

/// Strip Python string quotes/prefixes from a docstring literal and trim.
fn clean_docstring(raw: &str) -> String {
    let t = raw.trim();
    // Drop a leading string prefix (r, b, f, u, and combinations), case-insensitive.
    let after_prefix = {
        let bytes = t.as_bytes();
        let mut i = 0;
        while i < bytes.len() && i < 2 && matches!(bytes[i], b'r' | b'R' | b'b' | b'B' | b'f' | b'F' | b'u' | b'U') {
            i += 1;
        }
        &t[i..]
    };
    let body = after_prefix
        .strip_prefix("\"\"\"")
        .and_then(|s| s.strip_suffix("\"\"\""))
        .or_else(|| after_prefix.strip_prefix("'''").and_then(|s| s.strip_suffix("'''")))
        .or_else(|| after_prefix.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .or_else(|| after_prefix.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(after_prefix);
    body.trim().to_string()
}

/// Visibility from the underscore-prefix convention: a name that does not start
/// with `_` is a public API (`"pub"`); an underscore-prefixed name (`_x`,
/// `__x`, dunders) is private (`""`).
fn visibility_of(simple_name: &str) -> String {
    if simple_name.starts_with('_') {
        String::new()
    } else {
        "pub".to_string()
    }
}

fn qualify(type_prefix: Option<&str>, name: &str) -> String {
    match type_prefix {
        Some(t) if !t.is_empty() => format!("{t}::{name}"),
        _ => name.to_string(),
    }
}
