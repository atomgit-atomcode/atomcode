package com.atomcode.jetbrains.context

import com.atomcode.jetbrains.security.PathSensitivity

data class EditorContextState(
    val path: String? = null,
    val relativePath: String? = null,
    val language: String? = null,
    val selectionStartLine: Int? = null,
    val selectionEndLine: Int? = null,
    val hasSelection: Boolean = false,
    val sensitivity: PathSensitivity = PathSensitivity.Normal,
    val dirty: Boolean = false,
)

data class ContextCollectionRequest(
    val includeProjectMetadata: Boolean,
    val includeCurrentFile: Boolean,
    val explicitItems: List<String>,
)
