use cyber_agent::types::{ImageContent, Message};
use cyber_agent::vision::{
    detect_image_mime, format_injected_vision_description, is_deepseek_provider,
    is_deepseek_vision_model, prepare_image_for_deepseek, probe_model_vision,
    resolve_prompt_placeholders, AttachedImage, VisionCapability, VisionConfig, VisionEngine,
    DEFAULT_IMAGE_TOKENS, MAX_IMAGE_BYTES,
};
use cyber_agent::{estimate_messages_tokens, openai::message_to_openai};
use cyber_core::ProviderConfig;

#[test]
fn test_vision_model_and_provider_detection() {
    assert!(is_deepseek_vision_model("deepseek-flash"));
    assert!(is_deepseek_vision_model("DeepSeek-Flash-Vision"));
    assert!(is_deepseek_vision_model("deepseek-vl-7b"));
    assert!(is_deepseek_vision_model("deepseek-v4-flash-vision-exp"));
    assert!(!is_deepseek_vision_model("deepseek-chat"));
    assert!(!is_deepseek_vision_model("deepseek-reasoner"));
    assert!(!is_deepseek_vision_model("gpt-4o"));

    let deepseek_official = ProviderConfig {
        base_url: "https://api.deepseek.com".into(),
        model: "deepseek-chat".into(),
        ..Default::default()
    };
    assert!(is_deepseek_provider(&deepseek_official));

    let siliconflow_deepseek = ProviderConfig {
        base_url: "https://api.siliconflow.cn/v1".into(),
        model: "deepseek-ai/DeepSeek-V3".into(),
        ..Default::default()
    };
    assert!(is_deepseek_provider(&siliconflow_deepseek));

    let openai_provider = ProviderConfig {
        base_url: "https://api.openai.com/v1".into(),
        model: "gpt-4o".into(),
        ..Default::default()
    };
    assert!(!is_deepseek_provider(&openai_provider));
}

#[test]
fn test_magic_bytes_and_size_limits() {
    let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
    assert_eq!(detect_image_mime(&png), Some("image/png"));

    let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    assert_eq!(detect_image_mime(&jpeg), Some("image/jpeg"));

    let gif = b"GIF89a\x01\x00\x01\x00";
    assert_eq!(detect_image_mime(gif), Some("image/gif"));

    let webp = b"RIFF\x18\x00\x00\x00WEBPVP8 ";
    assert_eq!(detect_image_mime(webp), Some("image/webp"));

    let non_image = b"PLAIN TEXT CONTENT";
    assert_eq!(detect_image_mime(non_image), None);
}

#[test]
fn test_oversized_image_rejected() {
    let temp_dir = std::env::temp_dir();
    let oversized_file = temp_dir.join("cyber_oversized_test.png");
    // 创建一个标称超出 32 MiB 的稀疏/大文件
    let f = std::fs::File::create(&oversized_file).unwrap();
    f.set_len((MAX_IMAGE_BYTES + 1024) as u64).unwrap();

    let res = prepare_image_for_deepseek("cyber_oversized_test.png", &temp_dir);
    assert!(res.is_err());
    let err_str = res.err().unwrap().to_string();
    assert!(err_str.contains("32 MiB 上限"));

    let _ = std::fs::remove_file(oversized_file);
}

#[test]
fn test_placeholder_resolution_and_filtering() {
    let temp_dir = std::env::temp_dir();
    let img1 = temp_dir.join("test_img_1.png");
    let img2 = temp_dir.join("test_img_2.png");
    let img3 = temp_dir.join("test_img_3.png");

    let valid_png = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    std::fs::write(&img1, valid_png).unwrap();
    std::fs::write(&img2, valid_png).unwrap();
    std::fs::write(&img3, valid_png).unwrap();

    let attached = vec![
        AttachedImage::new(1, &img1, "one.png"),
        AttachedImage::new(2, &img2, "two.png"),
        AttachedImage::new(3, &img3, "three.png"),
    ];

    // 1. 正常引用 1 和 2，删除 3
    let prompt1 = "比较 [image:1] 与 [image:2] 之间的差异";
    let (p1, imgs1) = resolve_prompt_placeholders(prompt1, &attached, &temp_dir).unwrap();
    assert_eq!(p1, "比较 [image:1] 与 [image:2] 之间的差异");
    assert_eq!(imgs1.len(), 2);
    assert_eq!(imgs1[0].id, Some(1));
    assert_eq!(imgs1[1].id, Some(2));

    // 2. 混合显式路径与 Markdown 图像
    let prompt2 = format!(
        "分析本地图 [image: {}] 与外链 ![cat](https://example.com/cat.jpg)",
        img1.display()
    );
    let (p2, imgs2) = resolve_prompt_placeholders(&prompt2, &[], &temp_dir).unwrap();
    assert_eq!(p2, "分析本地图 [image:1] 与外链 [image:2]");
    assert_eq!(imgs2.len(), 2);
    assert_eq!(imgs2[0].id, Some(1));
    assert!(imgs2[0].url.starts_with("data:image/png;base64,"));
    assert_eq!(imgs2[1].id, Some(2));
    assert_eq!(imgs2[1].url, "https://example.com/cat.jpg");

    // 3. 重复占位符去重（prompt 内多次出现 [image:1]）
    let prompt3 = "看 [image:1]，然后再看一次 [image:1]";
    let (p3, imgs3) = resolve_prompt_placeholders(prompt3, &attached, &temp_dir).unwrap();
    assert_eq!(p3, "看 [image:1]，然后再看一次 [image:1]");
    assert_eq!(imgs3.len(), 1, "同一图片不应在 payload 中重复传输");
    assert_eq!(imgs3[0].id, Some(1));

    let _ = std::fs::remove_file(img1);
    let _ = std::fs::remove_file(img2);
    let _ = std::fs::remove_file(img3);
}

#[test]
fn test_openai_serialization_and_protocol_guard() {
    let img1 = ImageContent::new("data:image/png;base64,AAAA")
        .with_id(1)
        .with_detail("auto");
    let img2 = ImageContent::new("https://example.com/b.png")
        .with_id(2)
        .with_detail("low");

    // User 消息序列化为多模态数组
    let user_msg =
        Message::user_with_images("图一 [image:1] 图二 [image:2]", vec![img1.clone(), img2]);
    let val_user = message_to_openai(user_msg);
    assert_eq!(val_user["role"], "user");
    let parts = val_user["content"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "图一 [image:1] 图二 [image:2]");
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    assert_eq!(parts[1]["image_url"]["detail"], "auto");
    assert_eq!(parts[2]["type"], "image_url");
    assert_eq!(parts[2]["image_url"]["url"], "https://example.com/b.png");
    assert_eq!(parts[2]["image_url"]["detail"], "low");

    // System 消息携带图片时坚决隔离为纯文本，拦截 image_url
    let sys_msg = Message::system("系统提示").with_image(img1.clone());
    let val_sys = message_to_openai(sys_msg);
    assert_eq!(val_sys["role"], "system");
    assert_eq!(val_sys["content"], "系统提示");
    assert!(val_sys.get("image_url").is_none());

    // Assistant 消息携带图片时坚决隔离为纯文本
    let asst_msg = Message::assistant("回答文本").with_image(img1.clone());
    let val_asst = message_to_openai(asst_msg);
    assert_eq!(val_asst["role"], "assistant");
    assert_eq!(val_asst["content"], "回答文本");
    assert!(val_asst.get("image_url").is_none());

    // Tool 消息携带图片时坚决隔离为纯文本
    let tool_msg = Message::tool("c1", "执行输出").with_image(img1);
    let val_tool = message_to_openai(tool_msg);
    assert_eq!(val_tool["role"], "tool");
    assert_eq!(val_tool["content"], "执行输出");
    assert!(val_tool.get("image_url").is_none());
}

#[test]
fn test_image_token_estimation() {
    let m_text = Message::user("这是一个测试问题");
    let base_tokens = estimate_messages_tokens(&[m_text]);

    let img1 = ImageContent::new("data:image/png;base64,123");
    let m_one_img = Message::user_with_images("这是一个测试问题", vec![img1.clone()]);
    let one_img_tokens = estimate_messages_tokens(&[m_one_img]);
    assert_eq!(one_img_tokens, base_tokens + DEFAULT_IMAGE_TOKENS);

    let m_two_imgs = Message::user_with_images("这是一个测试问题", vec![img1.clone(), img1]);
    let two_imgs_tokens = estimate_messages_tokens(&[m_two_imgs]);
    assert_eq!(two_imgs_tokens, base_tokens + 2 * DEFAULT_IMAGE_TOKENS);
}

#[tokio::test]
async fn test_probe_model_vision_supported_200() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"id":"test","choices":[{"message":{"content":"ok"}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        }
    });

    let cfg = ProviderConfig {
        kind: "openai".into(),
        base_url: format!("http://127.0.0.1:{port}"),
        api_key: "sk-test".into(),
        model: "custom-vision-model".into(),
        ..Default::default()
    };

    let cap = probe_model_vision(&cfg, "custom-vision-model")
        .await
        .unwrap();
    assert_eq!(cap, VisionCapability::Supported);
}

#[tokio::test]
async fn test_probe_model_vision_unsupported_400() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let body =
                r#"{"error":{"message":"The model does not support multimodal/image input."}}"#;
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        }
    });

    let cfg = ProviderConfig {
        kind: "openai".into(),
        base_url: format!("http://127.0.0.1:{port}"),
        api_key: "sk-test".into(),
        model: "text-only-model".into(),
        ..Default::default()
    };

    let cap = probe_model_vision(&cfg, "text-only-model").await.unwrap();
    assert_eq!(cap, VisionCapability::Unsupported);
}

#[tokio::test]
async fn test_vision_engine_describe_and_format() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"choices":[{"message":{"content":"这是图像中识别出的关键文本：FLAG{cyber_vision_success}"}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        }
    });

    let mut providers = cyber_core::ProvidersConfig::default();
    let v_provider = ProviderConfig {
        kind: "openai".into(),
        base_url: format!("http://127.0.0.1:{port}"),
        api_key: "sk-test".into(),
        model: "vision-model".into(),
        ..Default::default()
    };
    providers.providers.insert("vision-p".into(), v_provider);

    let v_cfg = VisionConfig {
        enabled: true,
        provider: "vision-p".into(),
        model: "vision-model".into(),
        prompt: "分析此图片".into(),
        detail: "auto".into(),
    };

    let engine = VisionEngine::new(v_cfg);
    let img = ImageContent::new("data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==");
    let desc = engine.describe_images(&providers, &[img]).await.unwrap();
    assert!(desc.contains("FLAG{cyber_vision_success}"));

    let injected = format_injected_vision_description("请帮我提取图片中的 Flag", &desc);
    assert!(injected.contains("请帮我提取图片中的 Flag"));
    assert!(injected.contains("[系统自适应识图引擎已自动解析图片内容]"));
    assert!(injected.contains("FLAG{cyber_vision_success}"));
}
