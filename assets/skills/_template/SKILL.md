---
name: my-skill
description: 一句话描述这个 skill 做什么（注入工具 schema，供 LLM 判断是否调用）
triggers:
  - 关键词1
  - 关键词2
tools:
  - subfinder
  - httpx
# Claude Code 风格字段（可选）
allowed-tools:
  - read_file
  - shell
  - mcp__server__tool
disable-model-invocation: false
---

# 使用说明

这里写 skill 的详细使用说明（渐进式披露第二层）。

## 步骤

1. 第一步...
2. 第二步...

## 示例

```bash
subfinder -d example.com
```

## 注意事项

- 注意点 1
- 注意点 2
