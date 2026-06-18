package com.atomcode.jetbrains.ide

import com.intellij.openapi.editor.Editor
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.project.Project
import com.intellij.psi.PsiManager

class EditorContext(private val project: Project) {

    data class Selection(
        val text: String,
        val filePath: String,
        val fileName: String,
        val language: String?,
        val startLine: Int,
        val endLine: Int,
    )

    fun getCurrentSelection(): Selection? {
        val editor = FileEditorManager.getInstance(project).selectedTextEditor ?: return null
        val document = editor.document
        val model = editor.selectionModel
        if (!model.hasSelection()) return null

        val virtualFile = FileEditorManager.getInstance(project).selectedFiles.firstOrNull()
        val psiFile = virtualFile?.let { PsiManager.getInstance(project).findFile(it) }

        return Selection(
            text = model.selectedText ?: "",
            filePath = virtualFile?.path ?: "",
            fileName = virtualFile?.name ?: "untitled",
            language = psiFile?.language?.displayName,
            startLine = document.getLineNumber(model.selectionStart) + 1,
            endLine = document.getLineNumber(model.selectionEnd) + 1,
        )
    }

    fun getEditor(): Editor? =
        FileEditorManager.getInstance(project).selectedTextEditor

    fun getCurrentFilePath(): String? =
        FileEditorManager.getInstance(project).selectedFiles.firstOrNull()?.path

    fun getCurrentFileName(): String? =
        FileEditorManager.getInstance(project).selectedFiles.firstOrNull()?.name
}
