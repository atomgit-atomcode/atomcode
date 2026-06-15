package com.atomcode.jetbrains.services

import com.intellij.openapi.project.Project
import com.intellij.openapi.startup.StartupActivity

class AtomCodeStartupActivity : StartupActivity.DumbAware {
    override fun runActivity(project: Project) {
        AtomCodeProjectService.getInstance(project).startBackgroundHealthChecks()
    }
}
