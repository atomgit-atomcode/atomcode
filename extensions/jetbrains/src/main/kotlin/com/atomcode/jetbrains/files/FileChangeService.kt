package com.atomcode.jetbrains.files

import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.openapi.wm.ToolWindowManager
import java.io.File
import java.nio.file.Path
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

class FileChangeService(private val project: Project) {
    private val ioExecutor = java.util.concurrent.Executors.newCachedThreadPool { runnable ->
        Thread(runnable, "atomcode-file-io").apply { isDaemon = true }
    }
    fun refreshPath(path: String) {
        ApplicationManager.getApplication().invokeLater {
            if (project.isDisposed) return@invokeLater
            LocalFileSystem.getInstance().refreshAndFindFileByPath(path)
        }
    }

    fun openLocalChanges() {
        ApplicationManager.getApplication().invokeLater {
            if (project.isDisposed) return@invokeLater
            val manager = ToolWindowManager.getInstance(project)
            (manager.getToolWindow("Commit") ?: manager.getToolWindow("Version Control"))?.activate(null)
        }
    }

    fun openChangedFiles(limit: Int = 8): CompletableFuture<List<String>> =
        CompletableFuture.supplyAsync({
            val base = project.basePath ?: return@supplyAsync emptyList()
            val files = gitChangedFiles(base).take(limit)
            ApplicationManager.getApplication().invokeLater {
                if (project.isDisposed) return@invokeLater
                val localFileSystem = LocalFileSystem.getInstance()
                val editorManager = FileEditorManager.getInstance(project)
                files.forEach { relative ->
                    val path = Path.of(base, relative).normalize().toString()
                    localFileSystem.refreshAndFindFileByPath(path)?.let {
                        editorManager.openFile(it, true)
                    }
                }
                openLocalChanges()
            }
            files
        }, ioExecutor)

    fun resolveProjectPath(relativeOrAbsolute: String): String {
        val base = project.basePath ?: return relativeOrAbsolute
        val path = Path.of(relativeOrAbsolute)
        return if (path.isAbsolute) path.toString() else Path.of(base, relativeOrAbsolute).normalize().toString()
    }

    private fun gitChangedFiles(basePath: String): List<String> {
        val process = ProcessBuilder("git", "-C", basePath, "status", "--porcelain", "--untracked-files=all")
            .redirectError(ProcessBuilder.Redirect.DISCARD)
            .start()
        val output = process.inputStream.bufferedReader().use { it.readText() }
        val exited = process.waitFor(30, TimeUnit.SECONDS)
        if (!exited) {
            process.destroyForcibly()
            return emptyList()
        }
        if (process.exitValue() != 0) return emptyList()

        return output
            .lineSequence()
            .mapNotNull(::parsePorcelainPath)
            .filter { it.isNotBlank() && !it.endsWith(File.separator) }
            .distinct()
            .toList()
    }

    private fun parsePorcelainPath(line: String): String? =
        com.atomcode.jetbrains.files.parsePorcelainPath(line)
}

internal fun parsePorcelainPath(line: String): String? {
    if (line.length < 4) return null
    val raw = line.substring(3).trim()
    if (raw.isBlank()) return null
    return raw.substringAfter(" -> ").trim().trim('"')
}
