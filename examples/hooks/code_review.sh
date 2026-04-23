#!/bin/bash
# Hook: 代码质量检查
# Trigger: post_tool_execution
# 当编辑文件后检查代码质量，给出建议

INPUT=$(cat)

if command -v jq &> /dev/null; then
    TOOL_NAME=$(echo "$INPUT" | jq -r '.result_context.tool_name // empty')
    SUCCESS=$(echo "$INPUT" | jq -r '.result_context.success // empty')
    
    # 只在编辑工具成功后触发
    if [ "$TOOL_NAME" = "edit_file" ] && [ "$SUCCESS" = "true" ]; then
        TOOL_ARGS=$(echo "$INPUT" | jq -r '.result_context.tool_args // empty')
        FILE_PATH=$(echo "$TOOL_ARGS" | jq -r '.file_path // empty')
        
        # 检查文件扩展名
        if [[ "$FILE_PATH" == *.rs ]]; then
            # Rust 文件：提示运行 cargo clippy
            echo "warning: Consider running 'cargo clippy' on $FILE_PATH"
        elif [[ "$FILE_PATH" == *.py ]]; then
            # Python 文件：提示运行 pylint
            echo "warning: Consider running 'pylint $FILE_PATH'"
        elif [[ "$FILE_PATH" == *.ts ]] || [[ "$FILE_PATH" == *.tsx ]]; then
            # TypeScript 文件：提示运行 eslint
            echo "warning: Consider running 'eslint $FILE_PATH'"
        fi
    fi
fi

echo "ok"
