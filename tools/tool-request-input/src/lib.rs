wit_bindgen::generate!({
    path: "../../wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

struct Component;

impl Guest for Component {
    fn run(input: ToolInput) -> ToolResult {
        let data = input.data.as_deref().unwrap_or("{}");
        let prompt = serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .and_then(|v| {
                v.get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "Please provide your input:".to_string());

        let answer = murmur::task::task::request_input(&prompt);

        ToolResult {
            status: Status::Passed,
            summary: Some(format!("user answered: {answer}")),
            data: Some(answer),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(Component);
