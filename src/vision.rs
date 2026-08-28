// Scene-description call against an OpenAI-compatible chat/completions
// endpoint (DeepSeek, OpenAI, OpenRouter, ...) or a local Ollama instance.

use crate::config::VisionConfig;
use serde_json::json;

pub const VISION_PROMPT: &str = "Describe this photo in one short sentence: where it looks like it was taken (indoor/outdoor, type of place) and what is happening. Do not guess real names of any people -- describe them only generically (e.g. 'two adults', 'a child'). Answer in the same language as this instruction unless told otherwise.";

/// Reasoning models (e.g. deepseek-*-vision-exp) can spend the whole
/// max_tokens budget on an internal "reasoning_content" chain-of-thought and
/// leave the real "content" field empty, especially when finish_reason is
/// "length". The Python prototype fell back to dumping the *entire* raw
/// reasoning_content in that case, which produced multi-paragraph chain-of-
/// thought text in the CSV instead of a description. Here: only use it as a
/// last resort, and trim it down to something description-shaped (the last
/// non-empty line, capped in length) instead of the whole dump.
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

pub fn describe_openai_compatible(data_url: &str, cfg: &VisionConfig) -> String {
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
            return format!("[vision error: HTTP {} {}]", code, &msg[..msg.len().min(200)]);
        }
        Err(e) => return format!("[vision error: {}]", e),
    };

    let data: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => return format!("[vision error: bad JSON response: {}]", e),
    };

    let msg = &data["choices"][0]["message"];
    let content = msg["content"].as_str().unwrap_or("").trim();
    if !content.is_empty() {
        return content.to_string();
    }
    let reasoning = msg["reasoning_content"].as_str().unwrap_or("");
    tidy_reasoning_fallback(reasoning)
}

pub fn describe_ollama(data_url: &str, endpoint: &str, model: &str) -> String {
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
        Err(e) => return format!("[vision error: {}]", e),
    };
    let data: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => return format!("[vision error: bad JSON response: {}]", e),
    };
    data["response"].as_str().unwrap_or("").trim().to_string()
}
