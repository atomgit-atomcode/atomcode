package com.atomcode.jetbrains.ui.webview

import com.atomcode.jetbrains.store.ChatState
import com.atomcode.jetbrains.store.ChatViewModel
import com.atomcode.jetbrains.store.toViewModel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.map

/**
 * 智能渲染节流器：
 * - 阻塞性交互（权限、错误）→ 立即渲染
 * - 生成完成 → 立即渲染
 * - 流式文本 → 30fps throttle
 */
class RenderThrottler(
    private val render: (ChatViewModel) -> Unit
) {
    suspend fun observe(state: StateFlow<ChatState>) {
        state
            .map { it.toViewModel() }
            .collect { vm ->
                when {
                    vm.isWaitingPermission || vm.generationError != null -> render(vm)
                    !vm.isGenerating -> render(vm)
                    else -> {
                        render(vm)
                        delay(33)
                    }
                }
            }
    }
}
