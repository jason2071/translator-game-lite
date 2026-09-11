//! AI translation providers.
//!
//! Two wire formats are supported:
//! - Ollama (local `:11434` and cloud `ollama.com`): the **native** API is
//!   used because only it can disable thinking (`"think": false`) — the
//!   OpenAI-compatible `/v1` endpoint does not expose that switch.
//! - Everything else: the generic OpenAI-compatible
//!   `/v1/chat/completions`.
//!
//! The trait is intentionally sync: translation runs on plain worker
//! threads (bounded concurrency, cancelable between batches), so the app
//! stays tokio-free. Adding another provider later only means implementing
//! this trait — the pipeline never changes.

use anyhow::{anyhow, Context, Result};

/// Which wire protocol an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// Ollama native API (`/api/chat`, `/api/tags`).
    Ollama,
    /// Generic OpenAI-compatible (`/v1/chat/completions`, `/v1/models`).
    OpenAiCompatible,
}

/// Detect the protocol from a user-supplied endpoint.
pub fn endpoint_kind(endpoint: &str) -> EndpointKind {
    let e = endpoint.to_lowercase();
    if e.contains("ollama.com") || e.contains(":11434") {
        EndpointKind::Ollama
    } else {
        EndpointKind::OpenAiCompatible
    }
}

/// One batch item. `context` is the pre-formatted context block
/// (speaker / previous / next) for this item.
#[derive(Debug, Clone)]
pub struct RequestItem {
    pub id: String,
    pub text: String,
    pub context: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TranslationRequest {
    /// Fully-built system prompt (template already filled with languages
    /// and glossary).
    pub system_prompt: String,
    pub items: Vec<RequestItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedItem {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct TranslationResponse {
    pub translations: Vec<TranslatedItem>,
}

pub trait TranslationProvider: Send + Sync {
    fn translate(&self, request: &TranslationRequest) -> Result<TranslationResponse>;

    /// Model ids available on the server, sorted.
    fn list_models(&self) -> Result<Vec<String>>;
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// OpenAI-compatible base URL, e.g. `https://api.openai.com/v1`,
    /// `https://ollama.com`, `http://localhost:11434`.
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    pub timeout_secs: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            temperature: 0.3,
            timeout_secs: 180,
        }
    }
}

/// Any OpenAI-compatible endpoint, plus Ollama's native API.
pub struct OpenAiCompatibleProvider {
    config: ProviderConfig,
    agent: ureq::Agent,
}

impl OpenAiCompatibleProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build();
        Self { config, agent }
    }

    fn kind(&self) -> EndpointKind {
        endpoint_kind(&self.config.endpoint)
    }

    fn chat_url(&self) -> String {
        match self.kind() {
            EndpointKind::Ollama => native_url(&self.config.endpoint, "chat"),
            EndpointKind::OpenAiCompatible => openai_url(
                &self.config.endpoint,
                "chat/completions",
            ),
        }
    }

    fn models_url(&self) -> String {
        match self.kind() {
            EndpointKind::Ollama => native_url(&self.config.endpoint, "tags"),
            EndpointKind::OpenAiCompatible => openai_url(&self.config.endpoint, "models"),
        }
    }
}

impl TranslationProvider for OpenAiCompatibleProvider {
    fn translate(&self, request: &TranslationRequest) -> Result<TranslationResponse> {
        let items_json = serde_json::to_string(&serde_json::json!({
            "items": request.items.iter().map(|item| serde_json::json!({
                "id": item.id,
                "text": item.text,
                "context": item.context,
            })).collect::<Vec<_>>()
        }))?;
        let messages = serde_json::json!([
            { "role": "system", "content": request.system_prompt },
            { "role": "user", "content": items_json },
        ]);

        let body = match self.kind() {
            // Native Ollama API: `think: false` turns off reasoning output
            // (the /v1 OpenAI-compatible endpoint cannot do this).
            EndpointKind::Ollama => serde_json::json!({
                "model": self.config.model,
                "messages": messages,
                "stream": false,
                "think": false,
                "options": { "temperature": self.config.temperature },
            }),
            EndpointKind::OpenAiCompatible => serde_json::json!({
                "model": self.config.model,
                "temperature": self.config.temperature,
                "messages": messages,
            }),
        };

        let value = send_json(&self.agent, &self.chat_url(), &self.config.api_key, body)?;

        let content = match self.kind() {
            EndpointKind::Ollama => value.pointer("/message/content"),
            EndpointKind::OpenAiCompatible => value.pointer("/choices/0/message/content"),
        }
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow!("AI response has no message content"))?;

        let parsed = parse_content(content)
            .map_err(|e| anyhow!("AI returned an unusable payload: {e}"))?;
        Ok(parsed)
    }

    fn list_models(&self) -> Result<Vec<String>> {
        let url = self.models_url();
        let value = send_get(&self.agent, &url, &self.config.api_key)?;

        let ids: Vec<String> = match self.kind() {
            // {"models":[{"name":"llama3.1",...}, ...]}
            EndpointKind::Ollama => value
                .get("models")
                .and_then(|m| m.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|m| m.get("name").and_then(|v| v.as_str()))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            // {"data":[{"id":"gpt-4o-mini",...}, ...]}
            EndpointKind::OpenAiCompatible => value
                .get("data")
                .and_then(|d| d.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        };

        let mut models: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                // Drop obvious non-chat models from the dropdown.
                let l = id.to_lowercase();
                !["embed", "whisper", "tts", "dall-e", "moderation", "transcribe"]
                    .iter()
                    .any(|bad| l.contains(bad))
            })
            .collect();
        models.sort();
        models.dedup();
        Ok(models)
    }
}

/// POST a JSON body with optional bearer auth, returning the parsed JSON
/// response. HTTP errors include the server's own message (truncated).
fn send_json(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value> {
    let mut post = agent.post(url);
    if let Some(auth) = authorization_header(api_key) {
        post = post.set("Authorization", &auth);
    }
    match post.send_json(body) {
        Ok(response) => response
            .into_json()
            .map_err(|e| anyhow!("invalid AI response: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            Err(http_error(code, detail))
        }
        Err(e) => Err(anyhow!("AI request failed: {e}")),
    }
}

/// GET a JSON document with optional bearer auth.
fn send_get(agent: &ureq::Agent, url: &str, api_key: &str) -> Result<serde_json::Value> {
    let mut get = agent.get(url);
    if let Some(auth) = authorization_header(api_key) {
        get = get.set("Authorization", &auth);
    }
    match get.call() {
        Ok(response) => response
            .into_json()
            .map_err(|e| anyhow!("invalid AI response: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            Err(http_error(code, detail))
        }
        Err(e) => Err(anyhow!("AI request failed: {e}")),
    }
}

fn http_error(code: u16, detail: String) -> anyhow::Error {
    let detail: String = detail.chars().take(300).collect();
    anyhow!("AI request failed: HTTP {code}: {detail}")
}

/// The `Authorization` header value for an API key; `None` when no key is
/// configured (local providers).
fn authorization_header(api_key: &str) -> Option<String> {
    let key = api_key.trim();
    (!key.is_empty()).then(|| format!("Bearer {key}"))
}

/// Build the `/chat/completions` URL from whatever base the user typed.
///
/// Tolerates the common shapes: `https://api.openai.com` (path inserted as
/// `/v1`), `https://api.openai.com/v1`, and a full URL that already ends in
/// `/chat/completions`.
fn openai_url(endpoint: &str, path: &str) -> String {
    let base = endpoint.trim().trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        return base.to_string();
    }
    if let Some(scheme_end) = base.find("://") {
        let has_path = base[scheme_end + 3..].contains('/');
        if !has_path {
            return format!("{base}/v1/{path}");
        }
    }
    format!("{base}/{path}")
}

/// Ollama native API URL; a trailing `/v1` on the endpoint is removed since
/// the native API lives at the host root (`/api/...`).
fn native_url(endpoint: &str, path: &str) -> String {
    let base = endpoint.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/api/{path}")
}

/// Remove `<think>...</think>` reasoning blocks some models emit inside the
/// message content. An unterminated block means only thinking arrived.
fn strip_think(text: &str) -> String {
    let mut out = text.to_string();
    loop {
        let Some(start) = out.to_lowercase().find("<think>") else { break };
        let after = &out[start..];
        match after.to_lowercase().find("</think>") {
            Some(end_rel) => {
                let end = start + end_rel + "</think>".len();
                out = format!("{}{}", &out[..start], &out[end..]);
            }
            None => {
                // Only thinking and nothing else.
                out = out[..start].to_string();
                break;
            }
        }
    }
    out.trim().to_string()
}

/// Parse the model's message content: plain JSON or a ```json fence (with
/// optional surrounding prose), shaped
/// `{"translations":[{"id":"...","text":"..."}]}`. `<think>` blocks are
/// stripped first.
pub fn parse_content(content: &str) -> Result<TranslationResponse> {
    let candidate = strip_think(content);
    let mut candidate = candidate.as_str();
    if let Some(fence) = candidate.find("```") {
        let after = &candidate[fence + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let end = after
            .find("```")
            .context("response has an unterminated code fence")?;
        candidate = after[..end].trim();
    }

    let value: serde_json::Value =
        serde_json::from_str(candidate).context("response is not valid JSON")?;
    let items = value
        .get("translations")
        .and_then(|t| t.as_array())
        .ok_or_else(|| anyhow!("response is missing \"translations\" array"))?;

    let mut out = TranslationResponse::default();
    for item in items {
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("a translation item is missing its id"))?;
        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            // Models often answer with leading/trailing spaces, doubled
            // spaces or invisible filler characters — normalize so list
            // rendering stays left-aligned.
            .map(crate::core::source::clean_spaces)
            .ok_or_else(|| anyhow!("a translation item is missing its text"))?;
        out.translations.push(TranslatedItem {
            id: id.to_string(),
            text,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_header_skipped_without_key() {
        assert!(authorization_header("").is_none());
        assert!(authorization_header("   ").is_none());
        assert_eq!(
            authorization_header("sk-123"),
            Some("Bearer sk-123".to_string())
        );
    }

    #[test]
    fn ollama_endpoints_use_the_native_api() {
        assert_eq!(
            OpenAiCompatibleProvider::new(ProviderConfig {
                endpoint: "https://ollama.com".into(),
                ..Default::default()
            })
            .chat_url(),
            "https://ollama.com/api/chat"
        );
        assert_eq!(
            OpenAiCompatibleProvider::new(ProviderConfig {
                endpoint: "https://ollama.com/v1".into(),
                ..Default::default()
            })
            .chat_url(),
            "https://ollama.com/api/chat"
        );
        assert_eq!(
            OpenAiCompatibleProvider::new(ProviderConfig {
                endpoint: "http://localhost:11434".into(),
                ..Default::default()
            })
            .models_url(),
            "http://localhost:11434/api/tags"
        );
    }

    #[test]
    fn openai_compatible_endpoints_keep_the_v1_shape() {
        assert_eq!(
            OpenAiCompatibleProvider::new(ProviderConfig {
                endpoint: "https://api.openai.com/v1".into(),
                ..Default::default()
            })
            .chat_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            OpenAiCompatibleProvider::new(ProviderConfig {
                endpoint: "https://api.openai.com".into(),
                ..Default::default()
            })
            .chat_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            OpenAiCompatibleProvider::new(ProviderConfig {
                endpoint: "http://localhost:1234/v1".into(),
                ..Default::default()
            })
            .models_url(),
            "http://localhost:1234/v1/models"
        );
        assert_eq!(
            OpenAiCompatibleProvider::new(ProviderConfig {
                endpoint: " https://api.openai.com/v1/chat/completions ".into(),
                ..Default::default()
            })
            .chat_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn parse_content_strips_think_blocks() {
        let content = "<think>I should translate carefully</think>\n{\"translations\":[{\"id\":\"a\",\"text\":\"สวัสดี\"}]}";
        let parsed = parse_content(content).unwrap();
        assert_eq!(
            parsed.translations,
            vec![TranslatedItem { id: "a".into(), text: "สวัสดี".into() }]
        );

        // Unterminated block: nothing but thinking arrived.
        assert!(parse_content("<think>hmm").is_err());

        // Qwen3-style inline tag.
        let parsed = parse_content(
            "{\"translations\":[{\"id\":\"a\",\"text\":\"x\"}]}",
        )
        .unwrap();
        assert_eq!(parsed.translations[0].text, "x");
    }

    #[test]
    fn parses_plain_json_payload() {
        let content = r#"{"translations":[{"id":"a|1","text":"สวัสดี"},{"id":"a|2","text":"ลาก่อน"}]}"#;
        let parsed = parse_content(content).unwrap();
        assert_eq!(parsed.translations.len(), 2);
        assert_eq!(parsed.translations[0].id, "a|1");
        assert_eq!(parsed.translations[0].text, "สวัสดี");
    }

    #[test]
    fn parses_fenced_json_with_prose() {
        let content = "Here you go:\n```json\n{\"translations\":[{\"id\":\"x\",\"text\":\"y\"}]}\n```\nDone.";
        let parsed = parse_content(content).unwrap();
        assert_eq!(parsed.translations, vec![TranslatedItem { id: "x".into(), text: "y".into() }]);
    }

    #[test]
    fn rejects_malformed_payloads() {
        assert!(parse_content("no json at all").is_err());
        assert!(parse_content("{\"items\": []}").is_err());
        assert!(parse_content("{\"translations\":[{\"text\":\"no id\"}]}").is_err());
        assert!(parse_content("{\"translations\":[{\"id\":\"a\"}]}").is_err());
    }
}
