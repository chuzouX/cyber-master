//! DeepSeek 视觉模型 (deepseek-flash) 多模态图像适配与占位符处理模块。
//!
//! 核心职责：
//! 1. 占位符解析与模型端对齐：支持 `[image:N]` 序列占位符、`[image:path]` 显式占位符及 `![alt](path)` Markdown 图片语法；
//! 2. 图像标准化与验证：Magic Bytes 探测、32 MiB 单图限制校验、Base64 Data URI 标准化；
//! 3. 模型与服务商能力识别：`is_deepseek_vision_model` 与 `is_deepseek_provider`；
//! 4. 附件过滤：用户在输入框中删除的占位符对应的附件自动丢弃，不误传。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, Result};
use crate::types::ImageContent;

pub use cyber_core::{
    get_model_vision_capability, is_deepseek_provider, is_deepseek_vision_model,
    save_model_vision_capability, CapabilityStore, VisionCapability, VisionConfig,
};

/// 最小有效 1x1 RGBA PNG Data URI（仅 92 字节，格式标准且经所有厂商网关兼容校验）。
pub const TINY_PROBE_PNG_DATA_URI: &str =
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// 最小有效 1x1 RGBA PNG 纯 Base64 字符串。
pub const TINY_PROBE_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// DeepSeek 官方单图大小上限（32 MiB）。
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// DeepSeek 官方规定每张图片缩放后消耗的上限 Token 数。
pub const DEFAULT_IMAGE_TOKENS: usize = 1024;

/// 会话或当前输入框中附带的本地/缓存图像。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachedImage {
    /// 关联序号（从 1 起始单调递增，对应 [image:1] 等占位符）
    pub id: usize,
    /// 图像在本地文件系统中的绝对路径或缓存路径
    pub path: PathBuf,
    /// 显示名称（如原始文件名或 clip_timestamp_1.png）
    pub display_name: String,
}

impl AttachedImage {
    pub fn new(id: usize, path: impl Into<PathBuf>, display_name: impl Into<String>) -> Self {
        Self {
            id,
            path: path.into(),
            display_name: display_name.into(),
        }
    }
}

/// 通过文件头 Magic Bytes 识别常见图像的 MIME 类型。
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// 根据文件扩展名推断图像 MIME 类型。
pub fn mime_from_extension(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// 判断给定的文件路径是否具有图像扩展名。
pub fn is_image_extension(path: &Path) -> bool {
    mime_from_extension(path).is_some()
}

/// 准备用于 DeepSeek 视觉模型请求的 `ImageContent`。
///
/// 支持 HTTP(S) URL、已有 Data URI 或本地文件路径。对于本地图片，自动读取、校验尺寸并转为 Base64 Data URI。
pub fn prepare_image_for_deepseek(path_or_url: &str, cwd: &Path) -> Result<ImageContent> {
    let trimmed = path_or_url.trim();
    if trimmed.is_empty() {
        return Err(AgentError::Vision("图片路径或 URL 不能为空".into()));
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(ImageContent::new(trimmed).with_detail("auto"));
    }

    if trimmed.starts_with("data:image/") {
        let mime = trimmed
            .strip_prefix("data:")
            .and_then(|s| s.split(';').next())
            .unwrap_or("image/png");
        return Ok(ImageContent::new(trimmed)
            .with_media_type(mime)
            .with_detail("auto"));
    }

    let p = Path::new(trimmed);
    let full_path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };

    if !full_path.exists() {
        return Err(AgentError::Vision(format!(
            "图片文件不存在: {}",
            full_path.display()
        )));
    }

    let metadata = std::fs::metadata(&full_path)?;
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err(AgentError::Vision(format!(
            "图片文件大小 ({} 字节) 超出 DeepSeek 单图 32 MiB 上限",
            metadata.len()
        )));
    }

    let bytes = std::fs::read(&full_path)?;
    let mime = detect_image_mime(&bytes)
        .or_else(|| mime_from_extension(&full_path))
        .unwrap_or("image/png");

    use base64::Engine;
    let b64 = base64::prelude::BASE64_STANDARD.encode(&bytes);
    let data_uri = format!("data:{};base64,{}", mime, b64);
    Ok(ImageContent::new(data_uri)
        .with_media_type(mime)
        .with_detail("auto"))
}

#[derive(Debug)]
enum FoundPlaceholder<'a> {
    /// [image:1] 或 [image: path]
    ImageTag {
        start: usize,
        end: usize,
        spec: &'a str,
    },
    /// ![alt](path)
    MarkdownTag {
        start: usize,
        end: usize,
        url: &'a str,
    },
}

impl<'a> FoundPlaceholder<'a> {
    fn start(&self) -> usize {
        match self {
            Self::ImageTag { start, .. } => *start,
            Self::MarkdownTag { start, .. } => *start,
        }
    }

    fn end(&self) -> usize {
        match self {
            Self::ImageTag { end, .. } => *end,
            Self::MarkdownTag { end, .. } => *end,
        }
    }
}

/// 扫描 prompt 中出现的占位符。
fn scan_placeholders(prompt: &str) -> Vec<FoundPlaceholder<'_>> {
    let mut found = Vec::new();
    let bytes = prompt.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'[' {
            // 检查是否为 [image:...
            let rest = &prompt[i + 1..];
            let lower = rest.to_ascii_lowercase();
            if lower.starts_with("image:") {
                let tag_prefix_len = 1 + "image:".len();
                if let Some(close_rel) = prompt[i + tag_prefix_len..].find(']') {
                    let end = i + tag_prefix_len + close_rel + 1;
                    let spec = prompt[i + tag_prefix_len..end - 1].trim();
                    found.push(FoundPlaceholder::ImageTag {
                        start: i,
                        end,
                        spec,
                    });
                    i = end;
                    continue;
                }
            }
        } else if bytes[i] == b'!' && i + 1 < len && bytes[i + 1] == b'[' {
            // 检查是否为 ![alt](url)
            let alt_start = i + 2;
            if let Some(alt_close_rel) = prompt[alt_start..].find(']') {
                let paren_open = alt_start + alt_close_rel + 1;
                if paren_open < len && bytes[paren_open] == b'(' {
                    let url_start = paren_open + 1;
                    if let Some(paren_close_rel) = prompt[url_start..].find(')') {
                        let end = url_start + paren_close_rel + 1;
                        let url = prompt[url_start..end - 1].trim();
                        found.push(FoundPlaceholder::MarkdownTag { start: i, end, url });
                        i = end;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }

    // 按起始位置递增排序
    found.sort_by_key(|f| f.start());
    found
}

/// 解析用户输入 prompt 中的图片占位符与 Markdown 图像，转换为保留序列标识符的 prompt 文本与对应的 ImageContent 列表。
///
/// 规则：
/// 1. `[image:N]` 映射到 `attached` 中 `id == N` 的附件；
/// 2. `[image: path/url]` 或 `![alt](path/url)` 自动读取对应图像，分配新序号 N 并替换为 `[image:N]`；
/// 3. 若 `attached` 中的某个附件在文本中未被任何占位符引用（例如用户删除或 Backspace 退格），该附件自动过滤，不随请求发送；
/// 4. 同一图片若在文本中多次引用（例如两次 `[image:1]`），保留所有文本占位符，但底层消息 `images` 仅添加一次，杜绝重复 payload。
pub fn resolve_prompt_placeholders(
    prompt: &str,
    attached: &[AttachedImage],
    cwd: &Path,
) -> Result<(String, Vec<ImageContent>)> {
    let placeholders = scan_placeholders(prompt);
    if placeholders.is_empty() {
        return Ok((prompt.to_string(), Vec::new()));
    }

    let mut resolved_images: Vec<ImageContent> = Vec::new();
    let mut resolved_ids: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // 找出目前已存在的最大 id，以便给显式路径的图片分配自增 id
    let mut next_alloc_id = attached.iter().map(|a| a.id).max().unwrap_or(0);

    let mut new_prompt = String::with_capacity(prompt.len());
    let mut last_end = 0;

    for ph in placeholders {
        let (start, end) = (ph.start(), ph.end());
        new_prompt.push_str(&prompt[last_end..start]);
        last_end = end;

        match ph {
            FoundPlaceholder::ImageTag { spec, .. } => {
                // 判断 spec 是否为纯数字序号
                if let Ok(id) = spec.parse::<usize>() {
                    if let Some(att) = attached.iter().find(|a| a.id == id) {
                        if !resolved_ids.contains(&id) {
                            let mut content =
                                prepare_image_for_deepseek(&att.path.to_string_lossy(), cwd)?;
                            content.id = Some(id);
                            resolved_images.push(content);
                            resolved_ids.insert(id);
                        }
                        new_prompt.push_str(&format!("[image:{}]", id));
                    } else {
                        // 尝试作为本地文件名检查，若不存在则报未找到附件错误
                        let p = Path::new(spec);
                        let candidate = if p.is_absolute() {
                            p.to_path_buf()
                        } else {
                            cwd.join(p)
                        };
                        if candidate.exists() {
                            next_alloc_id += 1;
                            let id = next_alloc_id;
                            let mut content = prepare_image_for_deepseek(spec, cwd)?;
                            content.id = Some(id);
                            resolved_images.push(content);
                            resolved_ids.insert(id);
                            new_prompt.push_str(&format!("[image:{}]", id));
                        } else {
                            return Err(AgentError::Vision(format!(
                                "未找到序号为 {} 的图片附件 [image:{}]",
                                id, id
                            )));
                        }
                    }
                } else {
                    // spec 为路径或 URL
                    next_alloc_id += 1;
                    let id = next_alloc_id;
                    let mut content = prepare_image_for_deepseek(spec, cwd)?;
                    content.id = Some(id);
                    resolved_images.push(content);
                    resolved_ids.insert(id);
                    new_prompt.push_str(&format!("[image:{}]", id));
                }
            }
            FoundPlaceholder::MarkdownTag { url, .. } => {
                next_alloc_id += 1;
                let id = next_alloc_id;
                let mut content = prepare_image_for_deepseek(url, cwd)?;
                content.id = Some(id);
                resolved_images.push(content);
                resolved_ids.insert(id);
                new_prompt.push_str(&format!("[image:{}]", id));
            }
        }
    }

    new_prompt.push_str(&prompt[last_end..]);
    Ok((new_prompt, resolved_images))
}

/// 对指定服务商与模型发起真实的极小图像探针请求，精准测定其是否支持视觉/多模态输入。
pub async fn probe_model_vision(
    cfg: &cyber_core::ProviderConfig,
    model: &str,
) -> Result<VisionCapability> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()?;

    let endpoint = cfg.chat_endpoint();
    let headers = crate::models::fetch_headers(cfg);

    let payload = match cfg.kind.as_str() {
        "anthropic" => serde_json::json!({
            "model": model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "probe" },
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": TINY_PROBE_PNG_BASE64
                            }
                        }
                    ]
                }
            ],
            "max_tokens": 1
        }),
        "ollama" => serde_json::json!({
            "model": model,
            "messages": [
                {
                    "role": "user",
                    "content": "probe",
                    "images": [TINY_PROBE_PNG_BASE64]
                }
            ],
            "stream": false
        }),
        _ => serde_json::json!({
            "model": model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "probe" },
                        {
                            "type": "image_url",
                            "image_url": { "url": TINY_PROBE_PNG_DATA_URI }
                        }
                    ]
                }
            ],
            "max_tokens": 1,
            "stream": false
        }),
    };

    let resp = match client
        .post(&endpoint)
        .headers(headers)
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(AgentError::Provider(format!("探针请求网络错误: {e}"))),
    };

    let status = resp.status();
    if status.is_success() {
        return Ok(VisionCapability::Supported);
    }

    let err_body = resp.text().await.unwrap_or_default().to_ascii_lowercase();
    if (status.as_u16() == 400 || status.as_u16() == 422)
        && (err_body.contains("image")
            || err_body.contains("multimodal")
            || err_body.contains("vision")
            || err_body.contains("not support")
            || err_body.contains("unsupported")
            || err_body.contains("does not support"))
    {
        return Ok(VisionCapability::Unsupported);
    }

    Err(AgentError::Provider(format!(
        "探针返回异常状态 {status}: {err_body}"
    )))
}

/// 从 Data URI 或 URL 提取纯 Base64 数据串。
pub fn extract_base64_data(url_or_data: &str) -> &str {
    if let Some(pos) = url_or_data.find(";base64,") {
        &url_or_data[pos + 8..]
    } else {
        url_or_data
    }
}

/// 从各家接口响应提取纯文本分析结果。
pub fn extract_response_text(kind: &str, resp: &serde_json::Value) -> String {
    match kind {
        "anthropic" => {
            if let Some(content_arr) = resp.get("content").and_then(|c| c.as_array()) {
                let texts: Vec<&str> = content_arr
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                            b.get("text").and_then(|t| t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                if !texts.is_empty() {
                    return texts.join("\n");
                }
            }
        }
        "ollama" => {
            if let Some(content) = resp
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
            {
                return content.to_string();
            }
        }
        _ => {
            if let Some(content) = resp
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|choice| choice.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
            {
                return content.to_string();
            }
        }
    }
    resp.get("text")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| resp.to_string())
}

/// 格式化注入主模型上下文的识图分析文本。
pub fn format_injected_vision_description(user_prompt: &str, descriptions: &str) -> String {
    format!("{user_prompt}\n\n---\n[系统自适应识图引擎已自动解析图片内容]:\n{descriptions}\n---")
}

/// 识图引擎：当主模型不支持多模态视觉时，调用支持视觉的模型解析图片，将描述注入纯文本提示。
#[derive(Debug, Clone)]
pub struct VisionEngine {
    config: cyber_core::VisionConfig,
}

impl VisionEngine {
    pub fn new(config: cyber_core::VisionConfig) -> Self {
        Self { config }
    }

    /// 使用配置的识图模型对给定图片列表进行多模态分析，生成描述文本。
    pub async fn describe_images(
        &self,
        providers_cfg: &cyber_core::ProvidersConfig,
        images: &[ImageContent],
    ) -> Result<String> {
        if images.is_empty() {
            return Ok(String::new());
        }
        if !self.config.enabled {
            return Ok("[识图引擎已禁用，跳过图像分析]".to_string());
        }

        let (provider_cfg, model_name) = self.resolve_provider(providers_cfg)?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let endpoint = provider_cfg.chat_endpoint();
        let headers = crate::models::fetch_headers(&provider_cfg);

        let prompt_text = if self.config.prompt.trim().is_empty() {
            "请详细分析并描述此图片内容，提取其中的文本、界面元素与关键信息。"
        } else {
            &self.config.prompt
        };

        let payload = match provider_cfg.kind.as_str() {
            "anthropic" => {
                let mut content_parts: Vec<serde_json::Value> = vec![serde_json::json!({
                    "type": "text",
                    "text": prompt_text
                })];
                for img in images {
                    let base64_data = extract_base64_data(&img.url);
                    let media = img.media_type.as_deref().unwrap_or("image/png");
                    content_parts.push(serde_json::json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media,
                            "data": base64_data
                        }
                    }));
                }
                serde_json::json!({
                    "model": model_name,
                    "messages": [
                        {
                            "role": "user",
                            "content": content_parts
                        }
                    ],
                    "max_tokens": 2048
                })
            }
            "ollama" => {
                let img_base64s: Vec<String> = images
                    .iter()
                    .map(|img| extract_base64_data(&img.url).to_string())
                    .collect();
                serde_json::json!({
                    "model": model_name,
                    "messages": [
                        {
                            "role": "user",
                            "content": prompt_text,
                            "images": img_base64s
                        }
                    ],
                    "stream": false
                })
            }
            _ => {
                let mut content_parts: Vec<serde_json::Value> = vec![serde_json::json!({
                    "type": "text",
                    "text": prompt_text
                })];
                let detail = if self.config.detail.is_empty() {
                    "auto"
                } else {
                    &self.config.detail
                };
                for img in images {
                    content_parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": img.url,
                            "detail": detail
                        }
                    }));
                }
                serde_json::json!({
                    "model": model_name,
                    "messages": [
                        {
                            "role": "user",
                            "content": content_parts
                        }
                    ],
                    "max_tokens": 2048,
                    "stream": false
                })
            }
        };

        let resp = client
            .post(&endpoint)
            .headers(headers)
            .json(&payload)
            .send()
            .await
            .map_err(|e| AgentError::Provider(format!("识图引擎网络请求失败: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Provider(format!(
                "识图引擎请求失败 ({status}): {body}"
            )));
        }

        let resp_json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AgentError::Provider(format!("识图引擎响应解析失败: {e}")))?;

        let description = extract_response_text(&provider_cfg.kind, &resp_json);
        Ok(description)
    }

    /// 解析适用的识图服务商配置与模型名称。
    pub fn resolve_provider(
        &self,
        providers_cfg: &cyber_core::ProvidersConfig,
    ) -> Result<(cyber_core::ProviderConfig, String)> {
        // 1. 若配置中指定了 provider 且存在
        if !self.config.provider.is_empty() {
            if let Some(cfg) = providers_cfg.providers.get(&self.config.provider) {
                let model = if !self.config.model.is_empty() {
                    self.config.model.clone()
                } else {
                    cfg.model.clone()
                };
                return Ok((cfg.clone(), model));
            }
        }

        // 2. 遍历查找已知具有视觉能力或配置为 deepseek 的服务商
        for cfg in providers_cfg.providers.values() {
            if cyber_core::is_deepseek_provider(cfg)
                || cyber_core::is_deepseek_vision_model(&cfg.model)
                || cfg.models.values().any(|m| m.vision == Some(true))
            {
                let model = if !self.config.model.is_empty() {
                    self.config.model.clone()
                } else {
                    cfg.model.clone()
                };
                return Ok((cfg.clone(), model));
            }
        }

        // 3. 回退尝试 default_provider
        if let Some(cfg) = providers_cfg.providers.get(&providers_cfg.default_provider) {
            let model = if !self.config.model.is_empty() {
                self.config.model.clone()
            } else {
                cfg.model.clone()
            };
            return Ok((cfg.clone(), model));
        }

        Err(AgentError::Provider(
            "未找到可用的识图服务商，请在设置中配置 [agent.vision.provider]".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magic_bytes_detection() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        assert_eq!(detect_image_mime(&png), Some("image/png"));

        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(detect_image_mime(&jpeg), Some("image/jpeg"));

        let gif = b"GIF89a\x01\x00\x01\x00";
        assert_eq!(detect_image_mime(gif), Some("image/gif"));

        let webp = b"RIFF\x18\x00\x00\x00WEBPVP8 ";
        assert_eq!(detect_image_mime(webp), Some("image/webp"));

        let txt = b"hello world";
        assert_eq!(detect_image_mime(txt), None);
    }

    #[test]
    fn test_mime_from_extension() {
        assert_eq!(mime_from_extension(Path::new("pic.png")), Some("image/png"));
        assert_eq!(
            mime_from_extension(Path::new("PIC.JPG")),
            Some("image/jpeg")
        );
        assert_eq!(
            mime_from_extension(Path::new("a/b/c.webp")),
            Some("image/webp")
        );
        assert_eq!(
            mime_from_extension(Path::new("anim.gif")),
            Some("image/gif")
        );
        assert_eq!(mime_from_extension(Path::new("doc.pdf")), None);
    }

    #[test]
    fn test_prepare_image_http_url() {
        let cwd = Path::new(".");
        let res = prepare_image_for_deepseek("https://example.com/logo.png", cwd).unwrap();
        assert_eq!(res.url, "https://example.com/logo.png");
        assert_eq!(res.detail.as_deref(), Some("auto"));
    }

    #[test]
    fn test_prepare_image_local_file() {
        let temp_dir = std::env::temp_dir();
        let test_img_path = temp_dir.join("cyber_test_vision_pixel.png");
        // 1x1 最小有效 PNG 字节
        let png_bytes = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&test_img_path, png_bytes).unwrap();

        let cwd = &temp_dir;
        let res = prepare_image_for_deepseek("cyber_test_vision_pixel.png", cwd).unwrap();
        assert!(res.url.starts_with("data:image/png;base64,"));
        assert_eq!(res.media_type.as_deref(), Some("image/png"));
        assert_eq!(res.detail.as_deref(), Some("auto"));

        let _ = std::fs::remove_file(test_img_path);
    }

    #[test]
    fn test_resolve_prompt_placeholders_indexed_and_filtered() {
        let temp_dir = std::env::temp_dir();
        let img1_path = temp_dir.join("cyber_test_1.png");
        let img2_path = temp_dir.join("cyber_test_2.png");
        let png_bytes = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&img1_path, png_bytes).unwrap();
        std::fs::write(&img2_path, png_bytes).unwrap();

        let attached = vec![
            AttachedImage::new(1, &img1_path, "img1.png"),
            AttachedImage::new(2, &img2_path, "img2.png"),
        ];

        // 场景 1：用户引用了 [image:1] 与 [image:2]
        let prompt1 = "分析 [image:1] 并对照 [image:2]";
        let (p_res, imgs) = resolve_prompt_placeholders(prompt1, &attached, &temp_dir).unwrap();
        assert_eq!(p_res, "分析 [image:1] 并对照 [image:2]");
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].id, Some(1));
        assert_eq!(imgs[1].id, Some(2));

        // 场景 2：用户在输入框删掉了 [image:2]，只留下 [image:1]
        let prompt2 = "仅分析这张 [image:1]";
        let (p_res2, imgs2) = resolve_prompt_placeholders(prompt2, &attached, &temp_dir).unwrap();
        assert_eq!(p_res2, "仅分析这张 [image:1]");
        assert_eq!(imgs2.len(), 1);
        assert_eq!(imgs2[0].id, Some(1));

        // 场景 3：用户全部删除了占位符
        let prompt3 = "没有图片了";
        let (p_res3, imgs3) = resolve_prompt_placeholders(prompt3, &attached, &temp_dir).unwrap();
        assert_eq!(p_res3, "没有图片了");
        assert_eq!(imgs3.len(), 0);

        let _ = std::fs::remove_file(img1_path);
        let _ = std::fs::remove_file(img2_path);
    }

    #[test]
    fn test_resolve_prompt_explicit_path_and_markdown() {
        let temp_dir = std::env::temp_dir();
        let img_path = temp_dir.join("cyber_test_explicit.png");
        let png_bytes = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&img_path, png_bytes).unwrap();

        let prompt = format!(
            "显式路径 [image: {}] 与网络图 ![logo](https://example.com/logo.png)",
            img_path.display()
        );
        let (p_res, imgs) = resolve_prompt_placeholders(&prompt, &[], &temp_dir).unwrap();
        assert_eq!(p_res, "显式路径 [image:1] 与网络图 [image:2]");
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].id, Some(1));
        assert!(imgs[0].url.starts_with("data:image/png;base64,"));
        assert_eq!(imgs[1].id, Some(2));
        assert_eq!(imgs[1].url, "https://example.com/logo.png");

        let _ = std::fs::remove_file(img_path);
    }

    #[test]
    fn test_tiny_probe_constants_and_base64() {
        assert!(TINY_PROBE_PNG_DATA_URI.starts_with("data:image/png;base64,"));
        let extracted = extract_base64_data(TINY_PROBE_PNG_DATA_URI);
        assert_eq!(extracted, TINY_PROBE_PNG_BASE64);
    }

    #[test]
    fn test_format_injected_vision_description() {
        let prompt = "请分析这张图";
        let desc = "图中有红色圆圈与数字 42";
        let formatted = format_injected_vision_description(prompt, desc);
        assert!(formatted.starts_with("请分析这张图\n\n---"));
        assert!(formatted.contains("图中有红色圆圈与数字 42"));
    }

    #[test]
    fn test_extract_response_text_variants() {
        let openai_resp = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "OpenAI 识别结果"
                }
            }]
        });
        assert_eq!(
            extract_response_text("openai", &openai_resp),
            "OpenAI 识别结果"
        );

        let anthropic_resp = serde_json::json!({
            "content": [{
                "type": "text",
                "text": "Claude 识别结果"
            }]
        });
        assert_eq!(
            extract_response_text("anthropic", &anthropic_resp),
            "Claude 识别结果"
        );

        let ollama_resp = serde_json::json!({
            "message": {
                "content": "Ollama 识别结果"
            }
        });
        assert_eq!(
            extract_response_text("ollama", &ollama_resp),
            "Ollama 识别结果"
        );
    }

    #[test]
    fn test_vision_engine_resolve_provider() {
        let mut providers = cyber_core::ProvidersConfig::default();
        let p_deepseek = cyber_core::ProviderConfig {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
            ..Default::default()
        };
        providers.providers.insert("deepseek".into(), p_deepseek);

        let v_cfg = cyber_core::VisionConfig {
            enabled: true,
            provider: "deepseek".into(),
            model: "deepseek-flash".into(),
            prompt: "".into(),
            detail: "auto".into(),
        };
        let engine = VisionEngine::new(v_cfg);
        let (resolved_cfg, model) = engine.resolve_provider(&providers).unwrap();
        assert_eq!(model, "deepseek-flash");
        assert_eq!(resolved_cfg.base_url, "https://api.deepseek.com");
    }
}
