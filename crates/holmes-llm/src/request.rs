use holmes_core::{Message, ToolDefinition};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RequestContent {
    Text(String),
    Blocks(Vec<RequestContentBlock>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum RequestContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlData },
}

#[derive(Debug, Serialize)]
pub struct ImageUrlData {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<RequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct RequestMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<RequestContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<RequestToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RequestToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: RequestFunctionCall,
}

#[derive(Debug, Serialize)]
pub struct RequestFunctionCall {
    pub name: String,
    pub arguments: String,
}

impl ChatCompletionRequest {
    pub fn new(model: impl Into<String>, messages: &[Message], tools: &[ToolDefinition]) -> Self {
        let request_messages: Vec<RequestMessage> = messages
            .iter()
            .map(|m| RequestMessage {
                role: format!(
                    "{}",
                    serde_json::to_value(&m.role).unwrap().as_str().unwrap()
                ),
                content: if let Some(ref images) = m.image_blocks {
                    let text = m.content.clone().unwrap_or_default();
                    let mut blocks = vec![RequestContentBlock::Text { text }];
                    for (base64, media_type) in images {
                        blocks.push(RequestContentBlock::ImageUrl {
                            image_url: ImageUrlData {
                                url: format!("data:{};base64,{}", media_type, base64),
                            },
                        });
                    }
                    Some(RequestContent::Blocks(blocks))
                } else {
                    m.content.as_ref().map(|c| RequestContent::Text(c.clone()))
                },
                tool_calls: m.tool_calls.as_ref().map(|tcs| {
                    tcs.iter()
                        .map(|tc| RequestToolCall {
                            id: tc.id.clone(),
                            call_type: tc.call_type.clone(),
                            function: RequestFunctionCall {
                                name: tc.function.name.clone(),
                                arguments: tc.function.arguments.clone(),
                            },
                        })
                        .collect()
                }),
                tool_call_id: m.tool_call_id.clone(),
                name: m.name.clone(),
            })
            .collect();

        let tools_opt = if tools.is_empty() {
            None
        } else {
            Some(tools.to_vec())
        };

        Self {
            model: model.into(),
            messages: request_messages,
            tools: tools_opt,
            temperature: None,
            max_tokens: None,
        }
    }

    pub fn with_temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }

    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = Some(max);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::Message;

    #[test]
    fn request_serializes_without_tools() {
        let msgs = vec![Message::system("hello"), Message::user("world")];
        let req = ChatCompletionRequest::new("gpt-4", &msgs, &[]);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "gpt-4");
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn request_with_temperature() {
        let msgs = vec![Message::user("test")];
        let req = ChatCompletionRequest::new("model", &msgs, &[]).with_temperature(0.7);
        let json = serde_json::to_value(&req).unwrap();
        let temp = json["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.001);
    }
}
