//! inspect_image: 使用视觉多模态大模型（如 DeepSeek Flash）查看并深度分析本地图像或网络图片。

use std::future::Future;
use std::pin::Pin;

use futures::StreamExt;
use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::provider::{provider_factory, StreamRequest};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::types::{Message, StreamEvent};
use crate::vision::{is_deepseek_provider, is_deepseek_vision_model, prepare_image_for_deepseek};

pub struct InspectImageTool;

impl Tool for InspectImageTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "inspect_image".into(),
            description: "使用视觉多模态大模型（如 DeepSeek Flash）查看并深度分析本地图像或网络图片。可用于验证码识别、网络拓扑/图表分析、网页前端截图审计、隐写术/二维码/敏感信息提取及 CTF 图像理解。".into(),
            tags: vec!["vision".into(), "image".into(), "multimodal".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "本地图片路径（相对当前工作目录或绝对路径）或可直接访问的 HTTP(S) URL"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "分析指示或针对该图片提出的问题（默认：请详细描述并分析此图片内容，提取其中的文本、界面元素与安全关键信息）"
                    },
                    "detail": {
                        "type": "string",
                        "enum": ["low", "high", "original", "auto"],
                        "description": "DeepSeek 图片细节级别：low (省 token) / high / original (原图) / auto (默认)"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("inspect_image 缺少 path 参数".into()))?;
            let prompt = input
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("请详细描述并分析此图片内容，提取其中的文本、界面元素与安全关键信息");
            let detail = input
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or("auto");

            let mut image = match prepare_image_for_deepseek(path, &ctx.cwd) {
                Ok(img) => img,
                Err(err) => {
                    return Ok(ToolOutput {
                        content: format!("读取或处理图像失败: {err}"),
                        is_error: true,
                    });
                }
            };
            image.detail = Some(detail.to_string());

            if ctx.is_mock() {
                return Ok(ToolOutput {
                    content: format!(
                        "[Mock Vision Analysis for {}]: 成功解析图像数据（MIME: {}），针对提示「{}」完成安全视觉分析。",
                        path,
                        image.media_type.as_deref().unwrap_or("auto"),
                        prompt
                    ),
                    is_error: false,
                });
            }

            let provider_cfg = match ctx.provider_config() {
                Some(cfg) => cfg,
                None => {
                    return Ok(ToolOutput {
                        content: "未配置有效 Provider，无法调用视觉多模态大模型".into(),
                        is_error: true,
                    });
                }
            };

            let mut vision_cfg = provider_cfg.clone();
            let cap =
                cyber_core::get_model_vision_capability(&vision_cfg, "", &vision_cfg.model, None);
            // 若当前模型不支持视觉且为 DeepSeek 服务商，自动路由至 deepseek-flash
            if (!cap.is_supported() && is_deepseek_provider(&vision_cfg))
                || (is_deepseek_provider(&vision_cfg)
                    && !is_deepseek_vision_model(&vision_cfg.model))
            {
                vision_cfg.model = "deepseek-flash".to_string();
            }

            let provider = match provider_factory(&vision_cfg, false) {
                Ok(p) => p,
                Err(err) => {
                    return Ok(ToolOutput {
                        content: format!("创建视觉 Provider 失败: {err}"),
                        is_error: true,
                    });
                }
            };

            let msg = Message::user_with_images(prompt, vec![image]);
            let req = StreamRequest::new(vec![msg]);

            let mut stream = provider.stream(req);
            let mut full_text = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    StreamEvent::Delta(token) => full_text.push_str(&token),
                    StreamEvent::Error(err) => {
                        return Ok(ToolOutput {
                            content: format!("视觉模型分析失败: {err}"),
                            is_error: true,
                        });
                    }
                    _ => {}
                }
            }

            if full_text.trim().is_empty() {
                Ok(ToolOutput {
                    content: "（视觉模型返回内容为空）".into(),
                    is_error: false,
                })
            } else {
                Ok(ToolOutput {
                    content: full_text,
                    is_error: false,
                })
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::ProviderConfig;

    #[test]
    fn schema_matches_expected() {
        let tool = InspectImageTool;
        let schema = tool.schema();
        assert_eq!(schema.name, "inspect_image");
        assert!(schema.parameters["properties"].get("path").is_some());
        assert!(schema.parameters["properties"].get("prompt").is_some());
        assert!(schema.parameters["properties"].get("detail").is_some());
    }

    #[tokio::test]
    async fn mock_execution_succeeds() {
        let temp_dir = std::env::temp_dir();
        let img_file = temp_dir.join("mock_inspect_test.png");
        let png_bytes = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&img_file, png_bytes).unwrap();

        let ctx = ToolCtx::new(temp_dir.clone(), vec![], None, vec![]).with_mock(true);
        let out = InspectImageTool
            .run(
                json!({
                    "path": "mock_inspect_test.png",
                    "prompt": "识别图中验证码"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Mock Vision Analysis"));
        assert!(out.content.contains("识别图中验证码"));

        let _ = std::fs::remove_file(img_file);
    }

    #[tokio::test]
    async fn nonexistent_image_returns_error_output() {
        let temp_dir = std::env::temp_dir();
        let ctx = ToolCtx::new(temp_dir, vec![], None, vec![]).with_mock(true);
        let out = InspectImageTool
            .run(json!({"path": "definitely_not_exist_image_xyz.png"}), &ctx)
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("读取或处理图像失败"));
    }

    #[test]
    fn deepseek_reasoner_routed_to_deepseek_flash() {
        let p = ProviderConfig {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-reasoner".into(),
            ..Default::default()
        };
        let mut vision_cfg = p.clone();
        if is_deepseek_provider(&vision_cfg) && !is_deepseek_vision_model(&vision_cfg.model) {
            vision_cfg.model = "deepseek-flash".to_string();
        }
        assert_eq!(vision_cfg.model, "deepseek-flash");
    }
}
