//! The page as text: `Accessibility.getFullAXTree` rendered as an indented list with refs, and
//! the line diff a change summary is built from.

use std::collections::HashMap;

use serde_json::Value;

/// The longest snapshot returned, in characters.
pub const SNAPSHOT_MAX_CHARS: usize = 20_000;
/// The most `+`/`-` lines a change summary lists.
pub const DIFF_MAX_LINES: usize = 40;

/// Refs for one document: a DOM backend node id keeps its ref across snapshots, and a new node
/// takes the next number. A new document starts again at `e1`.
#[derive(Debug, Default)]
pub struct Refs {
    by_backend: HashMap<i64, u32>,
    by_ref: HashMap<u32, i64>,
    next: u32,
    document: Option<i64>,
}

impl Refs {
    /// Start over when `document` is not the one the refs were given out for. True when it was
    /// a new document.
    pub fn enter_document(&mut self, document: Option<i64>) -> bool {
        if document == self.document && self.next > 0 {
            return false;
        }
        let new = self.document.is_some() || self.next > 0;
        *self = Self {
            document,
            ..Self::default()
        };
        new || document.is_some()
    }

    pub fn document(&self) -> Option<i64> {
        self.document
    }

    fn assign(&mut self, backend: i64) -> u32 {
        if let Some(r) = self.by_backend.get(&backend) {
            return *r;
        }
        self.next += 1;
        self.by_backend.insert(backend, self.next);
        self.by_ref.insert(self.next, backend);
        self.next
    }

    /// The backend node id behind `eN`, if this document gave it out.
    pub fn backend(&self, r: u32) -> Option<i64> {
        self.by_ref.get(&r).copied()
    }

    /// The refs as `state.json` keeps them: `eN` → backend node id, the last number given, and
    /// the document they belong to.
    pub fn save(&self) -> (std::collections::BTreeMap<String, i64>, u32, Option<i64>) {
        let map = self
            .by_ref
            .iter()
            .map(|(r, b)| (format!("e{r}"), *b))
            .collect();
        (map, self.next, self.document)
    }

    /// The refs [`Refs::save`] gave; an entry that is not `eN` is dropped.
    pub fn restore(
        map: &std::collections::BTreeMap<String, i64>,
        next: u32,
        document: Option<i64>,
    ) -> Self {
        let mut refs = Self {
            document,
            next,
            ..Self::default()
        };
        for (name, backend) in map {
            if let Some(r) = crate::request::parse_ref(name) {
                refs.by_ref.insert(r, *backend);
                refs.by_backend.insert(*backend, r);
                refs.next = refs.next.max(r);
            }
        }
        refs
    }
}

#[derive(Debug, Clone)]
pub struct AxNode {
    role: String,
    name: String,
    ignored: bool,
    backend: Option<i64>,
    children: Vec<String>,
    states: Vec<String>,
}

/// The AX tree, by node id, with its root.
#[derive(Debug)]
pub struct AxTree {
    nodes: HashMap<String, AxNode>,
    root: Option<String>,
}

impl AxTree {
    pub fn parse(result: &Value) -> Self {
        let list = result
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut nodes = HashMap::new();
        let mut root = None;
        for n in &list {
            let Some(id) = n.get("nodeId").and_then(Value::as_str) else {
                continue;
            };
            if root.is_none() && n.get("parentId").is_none() {
                root = Some(id.to_string());
            }
            let text = |ptr: &str| {
                n.pointer(ptr)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let role = text("/role/value");
            let name = text("/name/value");
            let mut states = Vec::new();
            for p in n
                .get("properties")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let pname = p.get("name").and_then(Value::as_str).unwrap_or("");
                let v = p.pointer("/value/value").cloned().unwrap_or(Value::Null);
                let truthy = v == Value::Bool(true) || v == Value::String("true".into());
                match pname {
                    "focused" | "disabled" | "required" | "readonly" | "selected" | "pressed"
                    | "modal"
                        if truthy =>
                    {
                        states.push(pname.to_string())
                    }
                    "checked" if truthy => states.push("checked".into()),
                    "checked" if v == Value::String("mixed".into()) => states.push("mixed".into()),
                    "expanded" => states.push(if truthy { "expanded" } else { "collapsed" }.into()),
                    "invalid" if v.as_str().is_some_and(|s| s != "false") => {
                        states.push("invalid".into())
                    }
                    "level" => {
                        if let Some(l) = v.as_u64() {
                            states.push(format!("level={l}"));
                        }
                    }
                    _ => {}
                }
            }
            let value = match n.pointer("/value/value") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(num)) => num.to_string(),
                _ => String::new(),
            };
            if !value.is_empty() && value != name {
                states.push(format!("value=\"{}\"", crate::util::one_line(&value, 200)));
            }
            nodes.insert(
                id.to_string(),
                AxNode {
                    role,
                    name,
                    ignored: n.get("ignored").and_then(Value::as_bool).unwrap_or(false),
                    backend: n.get("backendDOMNodeId").and_then(Value::as_i64),
                    children: n
                        .get("childIds")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect(),
                    states,
                },
            );
        }
        Self { nodes, root }
    }

    /// The document node's backend id, which changes exactly when a new document loads.
    pub fn document(&self) -> Option<i64> {
        self.root
            .as_ref()
            .and_then(|r| self.nodes.get(r))
            .and_then(|n| n.backend)
    }

    /// The AX node whose DOM backend id is `backend`.
    fn find_backend(&self, backend: i64) -> Option<&str> {
        self.nodes
            .iter()
            .find(|(_, n)| n.backend == Some(backend))
            .map(|(id, _)| id.as_str())
    }

    /// One line per listed node, giving each a ref. `under`, a backend id, renders that node's
    /// subtree only; `None` renders the whole page below its root.
    pub fn render(&self, refs: &mut Refs, under: Option<i64>) -> Option<Vec<String>> {
        let (start, fold_start) = match under {
            Some(b) => (self.find_backend(b)?.to_string(), false),
            None => (self.root.clone()?, true),
        };
        let mut lines = Vec::new();
        // (node id, depth, name of the nearest listed ancestor, whether to fold this node)
        let mut stack = vec![(start, 0usize, String::new(), fold_start)];
        while let Some((id, depth, parent_name, force_fold)) = stack.pop() {
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            if node.role == "InlineTextBox" {
                continue;
            }
            let role = display_role(&node.role);
            let folded = force_fold
                || node.ignored
                || role == "none"
                || role.is_empty()
                || (matches!(role, "generic" | "LabelText") && node.name.is_empty());
            let dropped_text =
                role == "text" && (node.name.trim().is_empty() || node.name == parent_name);
            let (child_depth, child_parent) = if folded || dropped_text {
                (depth, parent_name)
            } else {
                let mut line = format!("{}- {role}", "  ".repeat(depth));
                if !node.name.is_empty() {
                    line.push_str(&format!(" \"{}\"", crate::util::one_line(&node.name, 200)));
                }
                if !node.states.is_empty() {
                    line.push_str(&format!(" [{}]", node.states.join(", ")));
                }
                if let Some(b) = node.backend {
                    line.push_str(&format!(" [ref=e{}]", refs.assign(b)));
                }
                lines.push(line);
                (depth + 1, node.name.clone())
            };
            if dropped_text {
                continue;
            }
            for child in node.children.iter().rev() {
                stack.push((child.clone(), child_depth, child_parent.clone(), false));
            }
        }
        Some(lines)
    }
}

fn display_role(role: &str) -> &str {
    match role {
        "StaticText" => "text",
        "RootWebArea" => "document",
        other => other,
    }
}

/// The page line, then `lines`, cut at [`SNAPSHOT_MAX_CHARS`].
pub fn assemble(page_line: &str, lines: &[String]) -> String {
    let cut_line =
        format!("… cut at {SNAPSHOT_MAX_CHARS} characters; pass ref to snapshot one part");
    let mut out = page_line.to_string();
    let mut used = out.chars().count();
    for line in lines {
        let n = line.chars().count() + 1;
        if used + n > SNAPSHOT_MAX_CHARS {
            out.push('\n');
            out.push_str(&cut_line);
            return out;
        }
        out.push('\n');
        out.push_str(line);
        used += n;
    }
    out
}

pub fn page_line(title: &str, url: &str) -> String {
    format!("page: {} — {url}", crate::util::one_line(title, 200))
}

/// A line diff, as `(added, removed, lines)` where each line starts with `+ ` or `- `.
pub fn diff(old: &[String], new: &[String]) -> (usize, usize, Vec<String>) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[prefix..old.len() - suffix];
    let b = &new[prefix..new.len() - suffix];
    let mut ops = Vec::new();
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        ops.extend(a.iter().map(|l| format!("- {}", l.trim_start())));
        ops.extend(b.iter().map(|l| format!("+ {}", l.trim_start())));
    } else {
        // lcs[i][j] = LCS length of a[i..] and b[j..].
        let (n, m) = (a.len(), b.len());
        let mut lcs = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if a[i] == b[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && a[i] == b[j] {
                i += 1;
                j += 1;
            } else if j < m && (i == n || lcs[i][j + 1] > lcs[i + 1][j]) {
                ops.push(format!("+ {}", b[j].trim_start()));
                j += 1;
            } else {
                ops.push(format!("- {}", a[i].trim_start()));
                i += 1;
            }
        }
    }
    let added = ops.iter().filter(|l| l.starts_with('+')).count();
    let removed = ops.len() - added;
    (added, removed, ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> Value {
        json!({"nodes": [
            {"nodeId":"1","role":{"value":"RootWebArea"},"name":{"value":"Fixture"},"backendDOMNodeId":1,"childIds":["2"]},
            {"nodeId":"2","parentId":"1","role":{"value":"generic"},"name":{"value":""},"backendDOMNodeId":2,"childIds":["3","5","7"]},
            {"nodeId":"3","parentId":"2","role":{"value":"heading"},"name":{"value":"waiting"},"backendDOMNodeId":3,
             "properties":[{"name":"level","value":{"type":"integer","value":1}}],"childIds":["4"]},
            {"nodeId":"4","parentId":"3","role":{"value":"StaticText"},"name":{"value":"waiting"},"backendDOMNodeId":4,"childIds":[]},
            {"nodeId":"5","parentId":"2","role":{"value":"button"},"name":{"value":"Go"},"backendDOMNodeId":5,
             "properties":[{"name":"focused","value":{"type":"booleanOrUndefined","value":true}}],"childIds":["6"]},
            {"nodeId":"6","parentId":"5","ignored":true,"role":{"value":"none"},"backendDOMNodeId":6,"childIds":[]},
            {"nodeId":"7","parentId":"2","role":{"value":"textbox"},"name":{"value":"Name"},"value":{"value":"abc"},"backendDOMNodeId":7,"childIds":[]}
        ]})
    }

    #[test]
    fn renders_folds_and_gives_refs() {
        let t = AxTree::parse(&tree());
        let mut refs = Refs::default();
        assert!(refs.enter_document(t.document()));
        let lines = t.render(&mut refs, None).unwrap();
        assert_eq!(
            lines,
            vec![
                "- heading \"waiting\" [level=1] [ref=e1]",
                "- button \"Go\" [focused] [ref=e2]",
                "- textbox \"Name\" [value=\"abc\"] [ref=e3]",
            ]
        );
        // Same document: refs stay; a subtree renders on its own.
        assert!(!refs.enter_document(t.document()));
        assert_eq!(
            t.render(&mut refs, Some(5)).unwrap(),
            vec!["- button \"Go\" [focused] [ref=e2]"]
        );
        assert_eq!(refs.backend(2), Some(5));
        // A new document restarts at e1.
        assert!(refs.enter_document(Some(99)));
        assert_eq!(refs.backend(2), None);
    }

    #[test]
    fn refs_survive_a_save_and_restore() {
        let mut refs = Refs::default();
        refs.enter_document(Some(9));
        refs.assign(100);
        refs.assign(200);
        let (map, next, doc) = refs.save();
        assert_eq!(map.get("e2"), Some(&200));
        let mut back = Refs::restore(&map, next, doc);
        assert_eq!(
            (back.backend(1), back.backend(2), back.document()),
            (Some(100), Some(200), Some(9))
        );
        assert!(
            !back.enter_document(Some(9)),
            "the same document keeps its refs"
        );
        assert_eq!(back.assign(200), 2);
        assert_eq!(back.assign(300), 3);
    }

    #[test]
    fn assemble_cuts_with_a_closing_line() {
        let lines: Vec<String> = (0..2000).map(|i| format!("- text \"line {i}\"")).collect();
        let out = assemble("page: t — u", &lines);
        assert!(out.chars().count() <= SNAPSHOT_MAX_CHARS + 80);
        assert!(out.ends_with("… cut at 20000 characters; pass ref to snapshot one part"));
    }

    #[test]
    fn diff_lists_changes() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (a, r, lines) = diff(
            &s(&["x", "  - heading \"waiting\"", "z"]),
            &s(&["x", "  - heading \"clicked\"", "z"]),
        );
        assert_eq!((a, r), (1, 1));
        assert_eq!(
            lines,
            vec!["- - heading \"waiting\"", "+ - heading \"clicked\""]
        );
        assert_eq!(diff(&s(&["a"]), &s(&["a"])).2.len(), 0);
    }
}
