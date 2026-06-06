#![allow(dead_code)]

use crate::config::{ModelConfig, RESERVED_CHAT_COMPLETION_BODY_KEYS};
use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Map, Value};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct LabelRequest {
    pub mime_type: String,
    pub image_bytes: Vec<u8>,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelResponse {
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct OpenAiLabelClient {
    endpoint: Url,
    model: String,
    timeout: Duration,
    model_config: ModelConfig,
}

impl OpenAiLabelClient {
    pub fn new(config: &ModelConfig) -> Result<Self> {
        Ok(Self {
            endpoint: chat_completions_endpoint(&config.url)?,
            model: config.model.clone(),
            timeout: Duration::from_secs(config.timeout_seconds.max(1)),
            model_config: config.clone(),
        })
    }

    pub fn label_image(&self, request: LabelRequest) -> Result<LabelResponse> {
        let body = ChatCompletionRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage::system(request.prompt),
                ChatMessage::user(vec![
                    ContentPart::text("Label this image."),
                    ContentPart::image_url(data_url(&request.mime_type, &request.image_bytes)),
                ]),
            ],
            temperature: Some(0.0),
        };

        let request_body = build_request_body(&body, &self.model_config.extra_body)?;
        let response = send_http_request(
            self.endpoint.as_str(),
            &request_body,
            self.model_config.resolved_api_key(),
            self.timeout,
        )?;
        let completion: ChatCompletionResponse =
            serde_json::from_slice(&response).context("failed to parse chat completion")?;

        extract_text(&completion).map(|text| LabelResponse { text })
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: ChatContent,
}

impl ChatMessage {
    fn system(content: String) -> Self {
        Self {
            role: "system".to_string(),
            content: ChatContent::Text(content),
        }
    }

    fn user(parts: Vec<ContentPart>) -> Self {
        Self {
            role: "user".to_string(),
            content: ChatContent::Parts(parts),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ChatContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Serialize)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<ImageUrlPart>,
}

impl ContentPart {
    fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: Some(text.into()),
            image_url: None,
        }
    }

    fn image_url(url: String) -> Self {
        Self {
            kind: "image_url",
            text: None,
            image_url: Some(ImageUrlPart { url }),
        }
    }
}

#[derive(Debug, Serialize)]
struct ImageUrlPart {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<Value>,
}

fn build_request_body(
    body: &ChatCompletionRequest,
    extra_body: &Map<String, Value>,
) -> Result<Vec<u8>> {
    let mut request_body = serde_json::to_value(body)?
        .as_object()
        .cloned()
        .context("chat completion request body must serialize as a JSON object")?;
    merge_extra_body(&mut request_body, extra_body)?;
    serde_json::to_vec(&request_body).context("failed to serialize chat completion request body")
}

fn merge_extra_body(
    request_body: &mut Map<String, Value>,
    extra_body: &Map<String, Value>,
) -> Result<()> {
    for (key, value) in extra_body {
        if RESERVED_CHAT_COMPLETION_BODY_KEYS.contains(&key.as_str()) {
            anyhow::bail!(
                "model.extra_body must not override reserved chat-completion field {key:?}"
            );
        }
        request_body.insert(key.clone(), value.clone());
    }

    Ok(())
}

fn extract_text(response: &ChatCompletionResponse) -> Result<String> {
    let choice = response
        .choices
        .first()
        .context("chat completion returned no choices")?;
    let content = choice
        .message
        .content
        .as_ref()
        .context("chat completion returned empty content")?;

    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        other => anyhow::bail!("unsupported chat completion content: {other}"),
    };

    let text = text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("model returned empty label text");
    }

    Ok(text)
}

fn send_http_request(
    url: &str,
    body: &[u8],
    api_key: Option<String>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let client = Client::builder()
        .timeout(timeout)
        .build()
        .context("failed to build HTTP client")?;
    let mut request = client
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .body(body.to_vec());

    if let Some(api_key) = api_key {
        request = request.header(AUTHORIZATION, format!("Bearer {api_key}"));
    }

    let response = request.send().context("failed to send HTTP request")?;
    let status = response.status();
    let bytes = response
        .bytes()
        .context("failed to read HTTP response body")?;

    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        anyhow::bail!(
            "label request failed with status {}: {text}",
            status.as_u16()
        );
    }

    Ok(bytes.to_vec())
}

fn chat_completions_endpoint(base_url: &str) -> Result<Url> {
    let mut url = Url::parse(base_url).with_context(|| format!("invalid model.url: {base_url}"))?;
    if url
        .path()
        .trim_end_matches('/')
        .ends_with("/chat/completions")
    {
        return Ok(url);
    }

    let path = format!("{}/chat/completions", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

fn data_url(mime_type: &str, image_bytes: &[u8]) -> String {
    format!(
        "data:{mime_type};base64,{}",
        BASE64_STANDARD.encode(image_bytes)
    )
}
