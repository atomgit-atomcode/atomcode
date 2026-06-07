#!/bin/bash
# Hook: 自动 Git 提交
# Trigger: post_turn
# 每 N 轮对话后自动提交代码变更

# 读取输入
INPUT=$(cat)

if command -v jq &> /dev/null; then
    TURN_NUMBER=$(echo "$INPUT" | jq -r '.hook_context.turn_number // 0')
    
    # 每 5 轮自动提交
    if [ $((TURN_NUMBER % 5)) -eq 0 ]; then
        # 检查是否有变更
        if git diff --quiet 2>/dev/null; then
            echo "ok"
            exit 0
        fi
        
        # 自动提交
        git add -A 2>/dev/null
        git commit -m "Auto-commit at turn $TURN_NUMBER [atomcode]" 2>/dev/null
        
        if [ $? -eq 0 ]; then
            echo "ok"
        else
            echo "warning: Auto-commit failed"
        fi
        exit 0
    fi
fi

echo "ok"
