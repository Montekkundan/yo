//! Minimal native client for Vercel AI Gateway's OpenAI-compatible API.
//!
//! The client deliberately keeps provider credentials and conversation storage out
//! of this module. It authenticates with an AI Gateway API key, always requests
//! that providers do not train on prompt data, and exposes both buffered and SSE
//! chat-completion APIs.

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://ai-gateway.vercel.sh/v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub type GatewayResult<T> = Result<T, GatewayError>;

#[derive(Debug)]
pub enum GatewayError {
    Transport(reqwest::Error),
    Decode {
        context: &'static str,
        source: serde_json::Error,
    },
    Authentication {
        details: String,
    },
    PaymentRequired {
        details: String,
    },
    RateLimited {
        details: String,
        retry_after: Option<String>,
    },
    Api {
        status: u16,
        details: String,
    },
    Capability {
        details: String,
    },
    Stream {
        details: String,
    },
}

impl GatewayError {
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Authentication { .. } => Some(401),
            Self::PaymentRequired { .. } => Some(402),
            Self::RateLimited { .. } => Some(429),
            Self::Api { status, .. } => Some(*status),
            Self::Transport(_)
            | Self::Decode { .. }
            | Self::Capability { .. }
            | Self::Stream { .. } => None,
        }
    }

    pub fn retry_after(&self) -> Option<&str> {
        match self {
            Self::RateLimited { retry_after, .. } => retry_after.as_deref(),
            _ => None,
        }
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(
                f,
                "could not reach Vercel AI Gateway: {error}. Check your network and gateway base URL"
            ),
            Self::Decode { context, source } => {
                write!(f, "AI Gateway returned invalid {context} JSON: {source}")
            }
            Self::Authentication { details } => write!(
                f,
                "AI Gateway authentication failed (401). Check or replace your AI Gateway API key. {details}"
            ),
            Self::PaymentRequired { details } => write!(
                f,
                "AI Gateway credits or budget are exhausted (402). Add credits or raise the key budget. {details}"
            ),
            Self::RateLimited {
                details,
                retry_after,
            } => {
                write!(f, "AI Gateway rate limit exceeded (429).")?;
                if let Some(value) = retry_after {
                    write!(f, " Retry after {value}.")?;
                } else {
                    write!(f, " Retry shortly or choose another model.")?;
                }
                write!(f, " {details}")
            }
            Self::Api { status, details } => write!(
                f,
                "AI Gateway request failed with HTTP {status}. Verify the model ID and request settings. {details}"
            ),
            Self::Capability { details } => {
                write!(f, "AI Gateway model capability check failed: {details}")
            }
            Self::Stream { details } => write!(f, "AI Gateway stream failed: {details}"),
        }
    }
}

impl Error for GatewayError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(source) => Some(source),
            Self::Decode { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for GatewayError {
    fn from(value: reqwest::Error) -> Self {
        Self::Transport(value)
    }
}

#[derive(Clone)]
pub struct GatewayClient {
    http: Client,
    api_key: String,
    base_url: String,
}

impl fmt::Debug for GatewayClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayClient")
            .field("api_key", &"[redacted]")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl GatewayClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("valid built-in Gateway HTTP client configuration"),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self::with_http_client(api_key, base_url, Client::new())
    }

    #[cfg(test)]
    fn with_http_client(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        http: Client,
    ) -> Self {
        Self {
            http,
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub async fn list_models(&self) -> GatewayResult<Vec<GatewayModel>> {
        let response = self
            .http
            .get(self.endpoint("models"))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        let response = require_success(response).await?;
        let body = response.bytes().await?;
        let list: ModelList = decode_json(&body, "model-list")?;
        Ok(list.data)
    }

    pub async fn chat(&self, request: &ChatRequest) -> GatewayResult<ChatResponse> {
        let wire = ChatRequestWire::new(request, false);
        let response = self
            .http
            .post(self.endpoint("chat/completions"))
            .bearer_auth(&self.api_key)
            .json(&wire)
            .send()
            .await?;
        let response = require_success(response).await?;
        let body = response.bytes().await?;
        decode_json(&body, "chat-completion")
    }

    /// Streams chat chunks to `on_chunk` and also returns an aggregated first choice.
    pub async fn stream_chat<F>(
        &self,
        request: &ChatRequest,
        mut on_chunk: F,
    ) -> GatewayResult<StreamedChatResponse>
    where
        F: FnMut(&ChatChunk),
    {
        let wire = ChatRequestWire::new(request, true);
        let response = self
            .http
            .post(self.endpoint("chat/completions"))
            .bearer_auth(&self.api_key)
            .json(&wire)
            .send()
            .await?;
        let response = require_success(response).await?;
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut accumulator = StreamAccumulator::default();

        while let Some(chunk) = bytes.next().await {
            for event in decoder.push(&chunk?)? {
                if process_stream_event(event, &mut accumulator, &mut on_chunk)? {
                    return Ok(accumulator.finish());
                }
            }
        }

        for event in decoder.finish()? {
            if process_stream_event(event, &mut accumulator, &mut on_chunk)? {
                return Ok(accumulator.finish());
            }
        }

        Err(GatewayError::Stream {
            details: "connection closed before the required `data: [DONE]` event".to_owned(),
        })
    }

    pub async fn embeddings(&self, request: &EmbeddingRequest) -> GatewayResult<EmbeddingResponse> {
        let wire = EmbeddingRequestWire::new(request);
        let response = self
            .http
            .post(self.endpoint("embeddings"))
            .bearer_auth(&self.api_key)
            .json(&wire)
            .send()
            .await?;
        let response = require_success(response).await?;
        let body = response.bytes().await?;
        decode_json(&body, "embedding")
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }
}

#[derive(Clone, Debug, Default)]
pub struct GatewayOptions {
    pub order: Vec<String>,
    pub only: Vec<String>,
    pub fallback_models: Vec<String>,
    pub sort: Option<ProviderSort>,
    pub zero_data_retention: bool,
    pub user: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSort {
    Cost,
    Ttft,
    Tps,
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: Option<ToolChoice>,
    /// OpenAI-compatible structured output configuration, including `json_schema`.
    pub response_format: Option<Value>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub gateway: GatewayOptions,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            response_format: None,
            temperature: None,
            max_tokens: None,
            gateway: GatewayOptions::default(),
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    pub fn with_response_format(mut self, response_format: Value) -> Self {
        self.response_format = Some(response_format);
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: MessageRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(MessageRole::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(MessageRole::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(MessageRole::Assistant, content)
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: Some(content.into()),
            name: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
        }
    }

    fn text(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDefinition,
}

impl ToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_owned(),
            function: FunctionDefinition {
                name: name.into(),
                description: Some(description.into()),
                parameters,
                strict: None,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Function(String),
}

impl Serialize for ToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::None => serializer.serialize_str("none"),
            Self::Required => serializer.serialize_str("required"),
            Self::Function(name) => serde_json::json!({
                "type": "function",
                "function": { "name": name }
            })
            .serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    /// JSON encoded arguments, matching the OpenAI-compatible wire format.
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChatChoice>,
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatChoice {
    pub index: usize,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatChunk {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChatChunkChoice>,
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatChunkChoice {
    pub index: usize,
    #[serde(default)]
    pub delta: ChatDelta,
    pub finish_reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChatDelta {
    #[serde(default)]
    pub role: Option<MessageRole>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionCallDelta>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FunctionCallDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct StreamedChatResponse {
    pub id: Option<String>,
    pub model: Option<String>,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayModel {
    pub id: String,
    #[serde(rename = "type", default)]
    pub model_type: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub created: Option<u64>,
    #[serde(default)]
    pub released: Option<u64>,
    #[serde(default)]
    pub owned_by: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub pricing: Option<Value>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: EmbeddingInput,
    pub encoding_format: Option<EmbeddingEncoding>,
    pub dimensions: Option<u32>,
    pub gateway: GatewayOptions,
}

impl EmbeddingRequest {
    pub fn new(model: impl Into<String>, input: EmbeddingInput) -> Self {
        Self {
            model: model.into(),
            input,
            encoding_format: None,
            dimensions: None,
            gateway: GatewayOptions::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Text(String),
    Texts(Vec<String>),
    Tokens(Vec<u32>),
    TokenArrays(Vec<Vec<u32>>),
}

impl From<String> for EmbeddingInput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for EmbeddingInput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingEncoding {
    Float,
    Base64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub object: String,
    pub model: String,
    #[serde(default)]
    pub data: Vec<EmbeddingData>,
    pub usage: TokenUsage,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub object: String,
    pub index: usize,
    pub embedding: EmbeddingVector,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingVector {
    Float(Vec<f32>),
    Base64(String),
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<GatewayModel>,
}

#[derive(Serialize)]
struct ChatRequestWire<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "is_empty_slice")]
    tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
    #[serde(rename = "providerOptions")]
    provider_options: ProviderOptionsWire<'a>,
}

impl<'a> ChatRequestWire<'a> {
    fn new(request: &'a ChatRequest, stream: bool) -> Self {
        Self {
            model: &request.model,
            messages: &request.messages,
            tools: &request.tools,
            tool_choice: request.tool_choice.as_ref(),
            response_format: request.response_format.as_ref(),
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            stream,
            provider_options: ProviderOptionsWire::new(&request.gateway),
        }
    }
}

#[derive(Serialize)]
struct EmbeddingRequestWire<'a> {
    model: &'a str,
    input: &'a EmbeddingInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding_format: Option<&'a EmbeddingEncoding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
    #[serde(rename = "providerOptions")]
    provider_options: ProviderOptionsWire<'a>,
}

impl<'a> EmbeddingRequestWire<'a> {
    fn new(request: &'a EmbeddingRequest) -> Self {
        Self {
            model: &request.model,
            input: &request.input,
            encoding_format: request.encoding_format.as_ref(),
            dimensions: request.dimensions,
            provider_options: ProviderOptionsWire::new(&request.gateway),
        }
    }
}

#[derive(Serialize)]
struct ProviderOptionsWire<'a> {
    gateway: GatewayOptionsWire<'a>,
}

impl<'a> ProviderOptionsWire<'a> {
    fn new(options: &'a GatewayOptions) -> Self {
        Self {
            gateway: GatewayOptionsWire {
                disallow_prompt_training: true,
                order: &options.order,
                only: &options.only,
                models: &options.fallback_models,
                sort: options.sort.as_ref(),
                zero_data_retention: options.zero_data_retention,
                user: options.user.as_deref(),
                tags: &options.tags,
            },
        }
    }
}

#[derive(Serialize)]
struct GatewayOptionsWire<'a> {
    #[serde(rename = "disallowPromptTraining")]
    disallow_prompt_training: bool,
    #[serde(skip_serializing_if = "is_empty_slice")]
    order: &'a [String],
    #[serde(skip_serializing_if = "is_empty_slice")]
    only: &'a [String],
    #[serde(skip_serializing_if = "is_empty_slice")]
    models: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    sort: Option<&'a ProviderSort>,
    #[serde(rename = "zeroDataRetention", skip_serializing_if = "is_false")]
    zero_data_retention: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
    #[serde(skip_serializing_if = "is_empty_slice")]
    tags: &'a [String],
}

fn is_empty_slice<T>(value: &&[T]) -> bool {
    value.is_empty()
}

fn is_false(value: &bool) -> bool {
    !*value
}

async fn require_success(response: Response) -> GatewayResult<Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await.unwrap_or_default();
    Err(api_error(status, &headers, &body))
}

fn api_error(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
    let details = extract_error_details(body);
    match status.as_u16() {
        401 => GatewayError::Authentication { details },
        402 => GatewayError::PaymentRequired { details },
        429 => GatewayError::RateLimited {
            details,
            retry_after: headers
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        },
        code => GatewayError::Api {
            status: code,
            details,
        },
    }
}

fn extract_error_details(body: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(message) = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| value.get("message").and_then(Value::as_str))
            .or_else(|| value.get("error").and_then(Value::as_str))
        {
            return message.to_owned();
        }
    }

    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "No additional details were returned.".to_owned()
    } else {
        trimmed.chars().take(1_000).collect()
    }
}

fn decode_json<T>(body: &[u8], context: &'static str) -> GatewayResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_slice(body).map_err(|source| GatewayError::Decode { context, source })
}

fn process_stream_event<F>(
    event: SsePayload,
    accumulator: &mut StreamAccumulator,
    on_chunk: &mut F,
) -> GatewayResult<bool>
where
    F: FnMut(&ChatChunk),
{
    match event {
        SsePayload::Done => Ok(true),
        SsePayload::Data(data) => {
            let chunk: ChatChunk = decode_json(data.as_bytes(), "stream-chunk")?;
            accumulator.push(&chunk);
            on_chunk(&chunk);
            Ok(false)
        }
    }
}

#[derive(Default)]
struct StreamAccumulator {
    id: Option<String>,
    model: Option<String>,
    content: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
}

impl StreamAccumulator {
    fn push(&mut self, chunk: &ChatChunk) {
        self.id.get_or_insert_with(|| chunk.id.clone());
        self.model.get_or_insert_with(|| chunk.model.clone());
        if chunk.usage.is_some() {
            self.usage = chunk.usage.clone();
        }

        for choice in chunk.choices.iter().filter(|choice| choice.index == 0) {
            if let Some(content) = &choice.delta.content {
                self.content.push_str(content);
            }
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason.clone();
            }
            for delta in &choice.delta.tool_calls {
                let call = self.tool_calls.entry(delta.index).or_default();
                if let Some(id) = &delta.id {
                    call.id.push_str(id);
                }
                if let Some(kind) = &delta.kind {
                    call.kind.push_str(kind);
                }
                if let Some(function) = &delta.function {
                    if let Some(name) = &function.name {
                        call.name.push_str(name);
                    }
                    if let Some(arguments) = &function.arguments {
                        call.arguments.push_str(arguments);
                    }
                }
            }
        }
    }

    fn finish(self) -> StreamedChatResponse {
        StreamedChatResponse {
            id: self.id,
            model: self.model,
            content: self.content,
            tool_calls: self
                .tool_calls
                .into_values()
                .map(|call| ToolCall {
                    id: call.id,
                    kind: if call.kind.is_empty() {
                        "function".to_owned()
                    } else {
                        call.kind
                    },
                    function: FunctionCall {
                        name: call.name,
                        arguments: call.arguments,
                    },
                })
                .collect(),
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

#[derive(Debug, PartialEq, Eq)]
enum SsePayload {
    Data(String),
    Done,
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> GatewayResult<Vec<SsePayload>> {
        self.buffer.extend_from_slice(bytes);
        self.drain_complete_events()
    }

    fn finish(&mut self) -> GatewayResult<Vec<SsePayload>> {
        let mut events = self.drain_complete_events()?;
        if !self.buffer.iter().all(u8::is_ascii_whitespace) {
            let pending = std::mem::take(&mut self.buffer);
            if let Some(event) = parse_sse_event(&pending)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn drain_complete_events(&mut self) -> GatewayResult<Vec<SsePayload>> {
        let mut events = Vec::new();
        while let Some((event_end, delimiter_len)) = find_sse_boundary(&self.buffer) {
            let event = self.buffer[..event_end].to_vec();
            self.buffer.drain(..event_end + delimiter_len);
            if let Some(event) = parse_sse_event(&event)? {
                events.push(event);
            }
        }
        Ok(events)
    }
}

fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    const DELIMITERS: [&[u8]; 5] = [b"\r\n\r\n", b"\r\n\n", b"\n\r\n", b"\n\n", b"\r\r"];
    let mut earliest: Option<(usize, usize)> = None;

    for delimiter in DELIMITERS {
        if let Some(index) = buffer
            .windows(delimiter.len())
            .position(|window| window == delimiter)
        {
            if earliest.is_none_or(|(current, _)| index < current) {
                earliest = Some((index, delimiter.len()));
            }
        }
    }
    earliest
}

fn parse_sse_event(event: &[u8]) -> GatewayResult<Option<SsePayload>> {
    let event = std::str::from_utf8(event).map_err(|error| GatewayError::Stream {
        details: format!("received a non-UTF-8 SSE event: {error}"),
    })?;
    let mut data_lines = Vec::new();

    for line in event.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "data" {
            data_lines.push("");
        } else if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }

    if data_lines.is_empty() {
        return Ok(None);
    }
    let data = data_lines.join("\n");
    if data.trim() == "[DONE]" {
        Ok(Some(SsePayload::Done))
    } else {
        Ok(Some(SsePayload::Data(data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    struct TestResponse {
        status: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        chunks: Vec<Vec<u8>>,
        chunked: bool,
    }

    impl TestResponse {
        fn json(body: Value) -> Self {
            Self {
                status: "200 OK",
                headers: vec![("Content-Type", "application/json")],
                chunks: vec![serde_json::to_vec(&body).unwrap()],
                chunked: false,
            }
        }

        fn stream(chunks: Vec<&'static [u8]>) -> Self {
            Self {
                status: "200 OK",
                headers: vec![("Content-Type", "text/event-stream")],
                chunks: chunks.into_iter().map(ToOwned::to_owned).collect(),
                chunked: true,
            }
        }
    }

    async fn spawn_server(response: TestResponse) -> (String, oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let _ = request_tx.send(request);

            let mut head = format!("HTTP/1.1 {}\r\nConnection: close\r\n", response.status);
            for (name, value) in response.headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }

            if response.chunked {
                head.push_str("Transfer-Encoding: chunked\r\n\r\n");
                socket.write_all(head.as_bytes()).await.unwrap();
                for chunk in response.chunks {
                    socket
                        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                        .await
                        .unwrap();
                    socket.write_all(&chunk).await.unwrap();
                    socket.write_all(b"\r\n").await.unwrap();
                    tokio::task::yield_now().await;
                }
                socket.write_all(b"0\r\n\r\n").await.unwrap();
            } else {
                let body: Vec<u8> = response.chunks.into_iter().flatten().collect();
                head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });

        (format!("http://{address}/v1"), request_rx)
    }

    async fn read_request(socket: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut scratch = [0_u8; 1024];
        let mut expected_len = None;

        loop {
            let read = socket.read(&mut scratch).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&scratch[..read]);

            if expected_len.is_none() {
                if let Some(header_end) = find_bytes(&request, b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    });
                    expected_len = Some(header_end + 4 + content_length.unwrap_or(0));
                }
            }

            if expected_len.is_some_and(|length| request.len() >= length) {
                break;
            }
        }
        request
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn request_json(request: &[u8]) -> Value {
        let body_start = find_bytes(request, b"\r\n\r\n").unwrap() + 4;
        serde_json::from_slice(&request[body_start..]).unwrap()
    }

    #[test]
    fn sse_decoder_handles_single_byte_boundaries_and_crlf() {
        let raw = concat!(
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hé\"},\"finish_reason\":null}]}\r\n\r\n",
            ": keepalive\n",
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for byte in raw.as_bytes() {
            events.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }
        events.extend(decoder.finish().unwrap());

        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], SsePayload::Data(_)));
        assert!(matches!(events[1], SsePayload::Data(_)));
        assert_eq!(events[2], SsePayload::Done);
    }

    #[tokio::test]
    async fn lists_models_with_bearer_auth_and_custom_base_url() {
        let response = TestResponse::json(json!({
            "object": "list",
            "data": [{"id": "anthropic/claude-sonnet-5", "owned_by": "anthropic"}]
        }));
        let (base_url, captured) = spawn_server(response).await;
        let client = GatewayClient::with_base_url("gateway-test-key", base_url);

        let models = client.list_models().await.unwrap();
        let request = String::from_utf8(captured.await.unwrap()).unwrap();

        assert_eq!(models[0].id, "anthropic/claude-sonnet-5");
        assert!(request.starts_with("GET /v1/models HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer gateway-test-key"));
    }

    #[tokio::test]
    async fn chat_sends_tools_privacy_flag_and_parses_tool_calls() {
        let response = TestResponse::json(json!({
            "id": "chat-1",
            "model": "anthropic/claude-sonnet-5",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "run_command", "arguments": "{\"command\":\"pwd\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }));
        let (base_url, captured) = spawn_server(response).await;
        let client = GatewayClient::with_base_url("gateway-test-key", base_url);
        let request = ChatRequest::new(
            "anthropic/claude-sonnet-5",
            vec![ChatMessage::user("where am I?")],
        )
        .with_tools(vec![ToolDefinition::function(
            "run_command",
            "Run a safe terminal command",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }),
        )])
        .with_response_format(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "memory_candidate",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {"memory": {"type": "string"}},
                    "required": ["memory"],
                    "additionalProperties": false
                }
            }
        }));

        let response = client.chat(&request).await.unwrap();
        let captured = captured.await.unwrap();
        let body = request_json(&captured);

        assert_eq!(response.choices[0].message.tool_calls[0].id, "call-1");
        assert_eq!(body["stream"], false);
        assert_eq!(body["tools"][0]["function"]["name"], "run_command");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "memory_candidate"
        );
        assert_eq!(
            body["providerOptions"]["gateway"]["disallowPromptTraining"],
            true
        );
    }

    #[tokio::test]
    async fn stream_chat_handles_arbitrary_http_chunks_and_done() {
        let response = TestResponse::stream(vec![
            b"da",
            b"ta: {\"id\":\"chat-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel",
            b"lo \"},\"finish_reason\":null}]}\n\n",
            b"data: {\"id\":\"chat-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"world\"},\"finish_reason\":\"stop\"}]}\r\n\r\n",
            b"data: [DO",
            b"NE]\n\n",
        ]);
        let (base_url, captured) = spawn_server(response).await;
        let client = GatewayClient::with_base_url("gateway-test-key", base_url);
        let request = ChatRequest::new("openai/gpt-test", vec![ChatMessage::user("hello")]);
        let mut observed = String::new();

        let result = client
            .stream_chat(&request, |chunk| {
                if let Some(content) = chunk.choices[0].delta.content.as_deref() {
                    observed.push_str(content);
                }
            })
            .await
            .unwrap();
        let body = request_json(&captured.await.unwrap());

        assert_eq!(observed, "hello world");
        assert_eq!(result.content, "hello world");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
        assert_eq!(body["stream"], true);
        assert!(body.get("response_format").is_none());
    }

    #[tokio::test]
    async fn creates_embeddings_and_parses_float_vectors() {
        let response = TestResponse::json(json!({
            "object": "list",
            "model": "openai/text-embedding-test",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
            "usage": {"prompt_tokens": 2, "total_tokens": 2}
        }));
        let (base_url, captured) = spawn_server(response).await;
        let client = GatewayClient::with_base_url("gateway-test-key", base_url);
        let request = EmbeddingRequest::new("openai/text-embedding-test", "remember this".into());

        let response = client.embeddings(&request).await.unwrap();
        let captured = captured.await.unwrap();
        let captured_text = String::from_utf8_lossy(&captured);
        let body = request_json(&captured);

        assert!(captured_text.starts_with("POST /v1/embeddings HTTP/1.1"));
        assert!(matches!(
            &response.data[0].embedding,
            EmbeddingVector::Float(values) if values == &[0.1, 0.2]
        ));
        assert_eq!(body["input"], "remember this");
        assert_eq!(
            body["providerOptions"]["gateway"]["disallowPromptTraining"],
            true
        );
    }

    #[test]
    fn maps_actionable_http_errors() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("12"));
        let body = br#"{"error":{"message":"slow down"}}"#;

        let unauthorized = api_error(StatusCode::UNAUTHORIZED, &headers, body);
        let payment = api_error(StatusCode::PAYMENT_REQUIRED, &headers, body);
        let limited = api_error(StatusCode::TOO_MANY_REQUESTS, &headers, body);
        let other = api_error(StatusCode::BAD_REQUEST, &headers, body);

        assert!(unauthorized.to_string().contains("Check or replace"));
        assert!(payment.to_string().contains("Add credits"));
        assert_eq!(limited.retry_after(), Some("12"));
        assert!(limited.to_string().contains("Retry after 12"));
        assert!(other.to_string().contains("Verify the model ID"));
    }

    #[tokio::test]
    #[ignore = "requires AI_GATEWAY_API_KEY and performs a live Gateway request"]
    async fn live_gateway_catalog_smoke_test() {
        let key = std::env::var("AI_GATEWAY_API_KEY")
            .expect("set AI_GATEWAY_API_KEY before running this ignored test");
        let models = GatewayClient::new(key).list_models().await.unwrap();
        assert!(!models.is_empty());
    }
}
