package com.atomcode.jetbrains.daemon

import com.google.gson.JsonParser
import java.nio.file.Files
import java.nio.file.Path

/**
 * 读取 daemon 写出的本地 token 文件 `~/.atomcode/daemon-<port>.json`。
 * daemon 是唯一 writer；本插件仅读取其中的 `token` 字段作为 Bearer 携带。
 *
 * 解析使用 Gson（插件已依赖），失败时一律返回 null。
 */
object DaemonTokenFile {
    private fun atomcodeHome(): Path {
        val env = System.getenv("ATOMCODE_HOME")?.takeIf { it.isNotEmpty() }
        return if (env != null) Path.of(env)
        else Path.of(System.getProperty("user.home"), ".atomcode")
    }

    fun read(port: Int): String? = try {
        val filePath = atomcodeHome().resolve("daemon-$port.json")
        val raw = Files.readString(filePath)
        val element = JsonParser.parseString(raw)
        if (!element.isJsonObject) return null
        val obj = element.asJsonObject
        val token = obj.get("token")?.takeIf { !it.isJsonNull }?.asString
        token?.takeIf { it.isNotEmpty() }
    } catch (_: Exception) {
        null
    }
}
