package com.atomcode.jetbrains.security

import java.security.SecureRandom
import java.util.Base64

enum class PathSensitivity {
    Normal,
    Warn,
    StrongConfirm,
    Block,
}

object AtomCodeTokenFactory {
    private val random = SecureRandom()

    fun createToken(): String {
        val bytes = ByteArray(32)
        random.nextBytes(bytes)
        return Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)
    }
}

object SensitivePathClassifier {
    private val blockedNamesLower = setOf(
        "id_rsa",
        "id_ed25519",
        ".netrc",
        "credentials",
        "kubeconfig",
    )

    private val strongConfirmNamesLower = setOf(
        ".env",
        ".npmrc",
        ".pypirc",
        ".yarnrc.yml",
        "settings.xml",
        "gradle.properties",
    )

    fun classify(path: String): PathSensitivity {
        val normalized = path.replace('\\', '/')
        val name = normalized.substringAfterLast('/')
        val lower = normalized.lowercase()
        val lowerName = name.lowercase()
        return when {
            lowerName in blockedNamesLower -> PathSensitivity.Block
            lower.endsWith(".pem") || lower.endsWith(".key") || lower.endsWith(".p12") ||
                lower.endsWith(".pfx") || lower.endsWith(".jks") -> PathSensitivity.Block
            "/.ssh/" in lower || "/.aws/" in lower || "/.gnupg/" in lower -> PathSensitivity.Block
            lowerName in strongConfirmNamesLower || lowerName.startsWith(".env.") -> PathSensitivity.StrongConfirm
            "/.terraform/" in lower || lower.endsWith(".tfstate") -> PathSensitivity.StrongConfirm
            "/.git/" in lower -> PathSensitivity.StrongConfirm
            lower.endsWith(".log") || lower.endsWith(".dump") || lower.endsWith(".bak") -> PathSensitivity.Warn
            else -> PathSensitivity.Normal
        }
    }
}

object SecretRedactor {
    private val patterns = listOf(
        Regex("""(?i)(authorization:\s*bearer\s+)[^\s]+"""),
        Regex("""(?i)(api[_-]?key["']?\s*[:=]\s*["']?)[^"'\s]+"""),
        Regex("""(?i)(token["']?\s*[:=]\s*["']?)[^"'\s]+"""),
        Regex("""(?i)(secret[_-]?key["']?\s*[:=]\s*["']?)[^"'\s]+"""),
        Regex("""(?i)(access[_-]?key["']?\s*[:=]\s*["']?)[^"'\s]+"""),
        Regex("""(?i)(auth[_-]?token["']?\s*[:=]\s*["']?)[^"'\s]+"""),
        Regex("""(?i)(client[_-]?secret["']?\s*[:=]\s*["']?)[^"'\s]+"""),
        Regex("""(?i)(private[_-]?key["']?\s*[:=]\s*["']?)[^"'\s]+"""),
        Regex("""(?i)(password["']?\s*[:=]\s*["']?)[^"'\s]+"""),
        Regex("""(?i)(credential["']?\s*[:=]\s*["']?)[^"'\s]+"""),
    )

    fun redact(text: String): String =
        patterns.fold(text) { current, pattern -> pattern.replace(current, "$1[REDACTED]") }
}

