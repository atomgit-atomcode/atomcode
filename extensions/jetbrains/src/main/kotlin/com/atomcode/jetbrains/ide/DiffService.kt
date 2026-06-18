package com.atomcode.jetbrains.ide

import com.intellij.diff.DiffContentFactory
import com.intellij.diff.DiffManager
import com.intellij.diff.contents.DocumentContent
import com.intellij.diff.requests.SimpleDiffRequest
import com.intellij.openapi.project.Project

/**
 * IntelliJ Diff View 封装。
 * 从 artifact/tool result 产生的内容映射到 IDE 内置 Diff 视图。
 */
object DiffService {

    fun showDiff(
        project: Project,
        title: String,
        originalContent: String,
        modifiedContent: String,
        originalTitle: String = "Original",
        modifiedTitle: String = "Modified",
    ) {
        val factory = DiffContentFactory.getInstance()
        val original: DocumentContent = factory.create(originalContent)
        val modified: DocumentContent = factory.create(modifiedContent)

        val request = SimpleDiffRequest(title, original, modified, originalTitle, modifiedTitle)
        DiffManager.getInstance().showDiff(project, request)
    }
}
