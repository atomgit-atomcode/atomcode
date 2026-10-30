package com.atomcode.jetbrains.actions

import kotlin.test.Test
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

/**
 * 每个注册的 intention 都必须带描述目录，否则 IDE 每次展示它都会打一条
 * `PluginException: Intention Description Dir URL is null ... description.html`
 * （实测日志里出现过 4 次）。这里同时钉住"资源在 classpath 上"和"plugin.xml 注册了
 * dirName"，避免有人只补文件不注册、或反过来。
 */
class IntentionDescriptionsTest {
    private val intentions = listOf(
        "ExplainSelectionIntention",
        "FixSelectionIntention",
        "OptimizeSelectionIntention",
        "AddContextIntention",
    )

    @Test
    fun `every intention ships a description`() {
        intentions.forEach { name ->
            val resource = javaClass.getResource("/intentionDescriptions/$name/description.html")
            assertNotNull(resource, "$name 缺少 description.html：IDE 会报 Intention Description Dir URL is null")
            assertTrue(resource.readText().isNotBlank(), "$name 的 description.html 不能为空")
        }
    }

    @Test
    fun `the descriptor registers every description directory`() {
        val descriptor = javaClass.getResource("/META-INF/plugin.xml")
        assertNotNull(descriptor, "plugin.xml 必须在 classpath 上")
        val xml = descriptor.readText()
        assertTrue(xml.contains("<intentionDescriptions>"), "缺少 intentionDescriptions 扩展点")
        intentions.forEach { name ->
            assertTrue(
                xml.contains("dirName=\"$name\""),
                "$name 没有在 <intentionDescriptions> 里注册",
            )
            assertTrue(
                xml.contains("className=\"com.atomcode.jetbrains.actions.$name\""),
                "$name 的注册缺少 className",
            )
        }
    }
}
