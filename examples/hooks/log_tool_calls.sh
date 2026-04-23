#!/bin/bash
# Hook: 工具调用日志记录
# Trigger: post_tool_execution
# 记录所有工具调用到日志文件

# 读取 stdin 的 JSON 输入
INPUT=$(cat)

# 解析关键信息（需要 jq，如果没有则跳过）
if command -v jq &> /dev/null; then
    TOOL_NAME=$(echo "$INPUT" | jq -r '.result_context.tool_name // empty')
    TOOL_ARGS=$(echo "$INPUT" | jq -r '.result_context.tool_args // empty')
    SUCCESS=$(echo "$INPUT" | jq -r '.result_context.success // empty')
    DURATION=$(echo "$INPUT" | jq -r '.result_context.duration_ms // empty')
    
    # 记录到日志文件
    LOG_DIR="$HOME/.atomcode/hooks-logs"
    mkdir -p "$LOG_DIR"
    LOG_FILE="$LOG_DIR/tool-calls.log"
    
    TIMESTAMP=$(date '+%Y-%m-%d %H:%M:%S')
    echo "[$TIMESTAMP] $TOOL_NAME (${DURATION}ms, success=$SUCCESS)" >> "$LOG_FILE"
fi

# 返回 ok 表示 hook 执行成功
echo "ok"
