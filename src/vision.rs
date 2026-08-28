// Scene-description call against an OpenAI-compatible chat/completions
// endpoint (DeepSeek, OpenAI, OpenRouter, ...) or a local Ollama instance.
// Extended (cs_26.08.28) to pull structured scene tags and any visible text
// (OCR) out of the SAME call instead of adding separate API calls/models.

use crate::config::VisionConfig;
use serde_json::json;

pub const VISION_PROMPT: &str = "Analyze this photo and respond in EXACTLY this three-line format, nothing else, in English:\nDESCRIPTION: <one short sentence: indoor/outdoor, type of place, what is happening>\nTAGS: <comma-separated short scene tags, e.g. indoor, outdoor, nature, city, beach, food, pet, vehicle, event, document, night>\nTEXT: <any clearly readable text visible in the photo (signs, labels, screens, documents), or 'none' if there is none>\nDo not guess real names of any people -- describe them only generically (e.g. 'two adults', 'a child').";

#[derive(Debug, Default, Clone)]
pub struct VisionResult {
    pub description: String,
    pub tags: String,
    pub ocr_text: String,
}

/// Reasoning models (e.g. deepseek-*-vision-exp) can spend the whole
/// max_tokens budget on an internal "reasoning_content" chain-of-thought and
/// leave the real "content" field empty, especially when finish_reason is
/// "length". Here that fallback text is trimmed to the last non-empty line
/// (capped in length) so at least something description-shaped lands in the
/// CSV, instead of a multi-paragraph chain-of-thought dump; tags/OCR text
/// are left empty in that case since there is no structured output at all.
fn tidy_reasoning_fallback(reasoning: &str) -> String {
    let last_line = reasoning
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut s = last_line.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ' || c == '*').to_string();
    if s.len() > 240 {
        s.truncate(240);
        s.push('\u{2026}'); // "..."
    }
    if s.is_empty() {
        "[no description -- model used the full token budget on internal reasoning]".to_string()
    } else {
        s
    }
}

/// Parse the DESCRIPTION:/TAGS:/TEXT: format requested in VISION_PROMPT. If
/// the model didn't follow the format at all (no DESCRIPTION: line found),
/// the whole response is used as the description and tags/OCR stay empty --
/// better a usable free-text description than nothing.
fn parse_structured(content: &str) -> VisionResult {
    let mut description = String::new();
    let mut tags = String::new();
    let mut ocr_text = String::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("DESCRIPTION:") {
            description = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("TAGS:") {
            tags = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("TEXT:") {
            let t = rest.trim();
            if !t.is_empty() && !t.eq_ignore_ascii_case("none") {
                ocr_text = t.to_string();
            }
        }
    }
    if description.is_empty() {
        description = content.trim().to_string();
    }
    VisionResult { description, tags, ocr_text }
}

pub fn describe_openai_compatible(data_url: &str, cfg: &VisionConfig) -> VisionResult {
    let body = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": VISION_PROMPT},
                {"type": "image_url", "image_url": {"url": data_url}},
            ],
        }],
    });

    let mut req = ureq::post(&cfg.endpoint)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(60));
    if !cfg.api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", cfg.api_key));
    }

    let resp = match req.send_json(body) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().unwrap_or_default();
            return VisionResult {
                description: format!("[vision error: HTTP {} {}]", code, &msg[..msg.len().min(200)]),
                ..Default::default()
            };
        }
        Err(e) => {
            return VisionResult { description: format!("[vision error: {}]", e), ..Default::default() };
        }
    };

    let data: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return VisionResult {
                description: format!("[vision error: bad JSON response: {}]", e),
                ..Default::default()
            };
        }
    };

    let msg = &data["choices"][0]["message"];
    let content = msg["content"].as_str().unwrap_or("").trim();
    if !content.is_empty() {
        return parse_structured(content);
    }
    let reasoning = msg["reasoning_content"].as_str().unwrap_or("");
    VisionResult { description: tidy_reasoning_fallback(reasoning), ..Default::default() }
}

pub fn describe_ollama(data_url: &str, endpoint: &str, model: &str) -> VisionResult {
    // Ollama's /api/generate takes raw base64 (no "data:...;base64," prefix).
    let b64 = data_url.splitn(2, ',').nth(1).unwrap_or(data_url);
    let body = json!({
        "model": model,
        "prompt": VISION_PROMPT,
        "images": [b64],
        "stream": false,
    });
    let url = format!("{}/api/generate", endpoint.trim_end_matches('/'));
    let req = ureq::post(&url).timeout(std::time::Duration::from_secs(120));
    let resp = match req.send_json(body) {
        Ok(r) => r,
        Err(e) => return VisionResult { description: format!("[vision error: {}]", e), ..Default::default() },
    };
    let data: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return VisionResult {
                description: format!("[vision error: bad JSON response: {}]", e),
                ..Default::default()
            };
        }
    };
    let content = data["response"].as_str().unwrap_or("").trim();
    parse_structured(content)
}
