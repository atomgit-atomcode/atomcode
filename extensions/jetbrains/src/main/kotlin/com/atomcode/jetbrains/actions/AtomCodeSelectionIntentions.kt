package com.atomcode.jetbrains.actions

import com.intellij.codeInsight.intention.IntentionAction
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.project.Project
import com.intellij.psi.PsiFile

abstract class AtomCodeSelectionIntention(
    private val title: String,
    private val instruction: String?,
) : IntentionAction {
    override fun getText(): String = title

    override fun getFamilyName(): String = "AtomCode"

    override fun isAvailable(project: Project, editor: Editor, file: PsiFile): Boolean =
        if (instruction == null) editor.selectionModel.hasSelection() || editor.document.text.isNotBlank()
        else EditorAtomCodeActions.canSendSelectedText(editor)

    override fun invoke(project: Project, editor: Editor, file: PsiFile) {
        if (instruction == null) {
            EditorAtomCodeActions.addEditorContext(project, editor)
        } else {
            EditorAtomCodeActions.sendSelectionCommand(project, editor, instruction)
        }
    }

    override fun startInWriteAction(): Boolean = false
}

class ExplainSelectionIntention : AtomCodeSelectionIntention(
    "AtomCode: Explain Selection",
    "Please explain this code. What does it do, and why?",
)

class FixSelectionIntention : AtomCodeSelectionIntention(
    "AtomCode: Fix Selection",
    "Please fix any bugs or issues in this code.",
)

class OptimizeSelectionIntention : AtomCodeSelectionIntention(
    "AtomCode: Optimize Selection",
    "Please optimize this code for better performance and readability.",
)

class AddContextIntention : AtomCodeSelectionIntention(
    "AtomCode: Add Selection/File as Context",
    null,
)
