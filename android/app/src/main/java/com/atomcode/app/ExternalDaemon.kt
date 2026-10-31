package com.atomcode.app

import android.util.Log
import java.io.File
import java.net.HttpURLConnection
import java.net.URL

/**
 * 发现并连接「外部守护进程」——在设备上跑的 `atomcode daemon`。
 *
 * Android 不允许应用枚举其它 uid 的进程（`/proc/<pid>` 与 `ps` 都受限），所以“发现
 * atc 进程”只能靠间接口索：
 *
 *  1. 汇合文件 `daemon-<port>.json`（daemon 自己写的 `{pid, port, token}`，0600），
 *     放在一个双方都读得到的目录里 —— 文件名给端口，内容给 token；
 *  2. loopback 端口上的 `/health` 指纹（`"service":"atomcode-daemon"`），用来确认
 *     “这个端口上跑的确实是我们”，顺带判活。
 *
 * 凭据的边界是刻意分开的：**只有 App 私有目录里的汇合文件被当作 token 来源**。共享
 * 存储上的文件谁都能读，把它当凭据等于把 token 交给所有有存储权限的应用；那里只取
 * 端口当线索，剩下的靠 `--no-auth` 或用户手动输入。
 */
object ExternalDaemon {
    private const val TAG = "AtomCode"

    /**
     * How long a liveness probe waits.
     *
     * This is loopback, so the answer is normally instant; 400 ms was tight
     * enough that a cold app start (the process is busy bringing up the runtime
     * and the WebView) produced a false "nothing answers" and the external
     * daemon was only found after a manual restart. One second still fails fast
     * against a dead port, because a refused connection returns immediately.
     */
    private const val PROBE_TIMEOUT_MS = 1000

    /** Pause before the single retry, so a starting daemon has a moment. */
    private const val RETRY_DELAY_MS = 250L

    /** 一个已确认可用的外部守护进程。 */
    data class Endpoint(val port: Int, val url: String)

    /**
     * 发现结果：要么得到一个可用端点，要么只拿到一个“要凭据”的端口。
     *
     * 后者需要人来决定（手动输入 token，或让 daemon 以 `--no-auth` 重起），所以不自动连。
     */
    data class Discovery(val endpoint: Endpoint?, val needsTokenPort: Int?)

    /**
     * `trusted` 里的汇合文件可以给 token（App 私有目录）；
     * `hint` 里的只能给端口（共享存储）。
     */
    /**
     * 去哪找汇合文件。
     *
     * 外部 daemon 用 `ATOMCODE_HOME` 定位产品目录，所以约定把它指到 App 的工作目录：
     * `ATOMCODE_HOME=/storage/emulated/0/AtomCode atomcode daemon --host 127.0.0.1 --port 8787`。
     * 目录下的 `.daemon/` 是第二种约定，留给不想把产品目录摊在根上的用法。
     */
    fun searchDirs(workspace: File): List<File> =
        listOf(workspace, File(workspace, ".daemon")).filter { it.isDirectory }

    fun discover(trusted: List<File>, hint: List<File>): Discovery {
        for (dir in trusted.distinctBy { it.absolutePath }) {
            for (file in handoffs(dir)) {
                val port = portOf(file) ?: continue
                val token = tokenOf(file)
                if (token == null) continue
                if (isAlive(port)) {
                    Log.i(TAG, "external daemon found on port $port (${file.name})")
                    return Discovery(Endpoint(port, url(port, token)), null)
                }
                Log.i(TAG, "${file.name} points at $port but nothing answers")
            }
        }
        for (dir in hint.distinctBy { it.absolutePath }) {
            for (file in handoffs(dir)) {
                val port = portOf(file) ?: continue
                if (!isAlive(port)) {
                    Log.i(TAG, "${file.name} points at $port but nothing answers")
                    continue
                }
                // 共享存储的 token 不采信：只认“它不需要 token”这一种免凭据情形。
                if (allowsAnonymousIndex(port)) {
                    Log.i(TAG, "external daemon on port $port accepts unauthenticated access")
                    return Discovery(Endpoint(port, "http://127.0.0.1:$port/"), null)
                }
                Log.i(TAG, "external daemon on port $port needs a token; not using ${file.name}")
                return Discovery(null, port)
            }
        }
        return Discovery(null, null)
    }

    /**
     * 这个端口上是不是我们的 daemon（同时也就是"它还活着"）。
     *
     * 探两次：冷启动时应用进程刚起来，第一轮连接经常赶在 daemon 的 accept 循环
     * 就绪之前超时，日志写"nothing answers"，人得手动重启一次才认出来。第一次
     * 失败后再试一次，代价只有一次毫秒级等待，换掉那次莫须有的重启。
     */
    fun isAlive(port: Int): Boolean {
        if (probeAlive(port)) return true
        try {
            Thread.sleep(RETRY_DELAY_MS)
        } catch (interrupted: InterruptedException) {
            Thread.currentThread().interrupt()
            return false
        }
        return probeAlive(port)
    }

    private fun probeAlive(port: Int): Boolean {
        val body = health(port) ?: return false
        return Regex("\"service\"\\s*:\\s*\"atomcode-daemon\"").containsMatchIn(body)
    }

    private fun url(port: Int, token: String) = "http://127.0.0.1:$port/?token=$token"

    private fun handoffs(dir: File): List<File> =
        dir.listFiles { file ->
            file.isFile && file.name.startsWith("daemon-") && file.name.endsWith(".json")
        }?.sortedBy { it.name } ?: emptyList()

    private fun portOf(file: File): Int? {
        val port = Regex("\"port\"\\s*:\\s*(\\d+)").find(read(file) ?: return null)
            ?.groupValues?.get(1)?.toIntOrNull() ?: return null
        return port.takeIf { it in 1..65535 }
    }

    private fun tokenOf(file: File): String? {
        val text = read(file) ?: return null
        return Regex("\"token\"\\s*:\\s*\"([^\"]+)\"").find(text)?.groupValues?.get(1)
    }

    private fun read(file: File): String? = try {
        file.readText()
    } catch (failure: Exception) {
        Log.i(TAG, "cannot read ${file.name}: ${failure.javaClass.simpleName}")
        null
    }

    /** 无需令牌就能打开首页吗（`--no-auth` 的 daemon 才会这样）。 */
    private fun allowsAnonymousIndex(port: Int): Boolean = try {
        val connection = (URL("http://127.0.0.1:$port/").openConnection() as HttpURLConnection).apply {
            connectTimeout = PROBE_TIMEOUT_MS
            readTimeout = PROBE_TIMEOUT_MS
        }
        try {
            connection.responseCode == 200
        } finally {
            connection.disconnect()
        }
    } catch (failure: Exception) {
        false
    }

    private fun health(port: Int): String? = try {
        val connection = (URL("http://127.0.0.1:$port/health").openConnection()
            as HttpURLConnection).apply {
            connectTimeout = PROBE_TIMEOUT_MS
            readTimeout = PROBE_TIMEOUT_MS
        }
        try {
            if (connection.responseCode != 200) {
                null
            } else {
                connection.inputStream.bufferedReader().use { it.readText() }
            }
        } finally {
            connection.disconnect()
        }
    } catch (failure: Exception) {
        null
    }
}
