package com.atomcode.jetbrains.ui

import com.atomcode.jetbrains.i18n.AtomCodeBundle
import java.util.Locale

internal data class GearMenuLabels(
    val connectStart: String,
    val provider: String,
    val createProvider: String,
    val editProvider: String,
    val deleteProvider: String,
    val thinkingSettings: String,
    val login: String,
    val codingPlanSetup: String,
    val sessionHistory: String,
    val renameSession: String,
    val deleteSession: String,
    val refreshSessions: String,
    val openChanges: String,
    val diagnostics: String,
    val settings: String,
)

internal fun gearMenuLabels(locale: Locale = Locale.getDefault()): GearMenuLabels =
    GearMenuLabels(
        connectStart = AtomCodeBundle.message(locale, "gear.connectStart"),
        provider = AtomCodeBundle.message(locale, "gear.provider"),
        createProvider = AtomCodeBundle.message(locale, "gear.createProvider"),
        editProvider = AtomCodeBundle.message(locale, "gear.editProvider"),
        deleteProvider = AtomCodeBundle.message(locale, "gear.deleteProvider"),
        thinkingSettings = AtomCodeBundle.message(locale, "gear.thinkingSettings"),
        login = AtomCodeBundle.message(locale, "gear.login"),
        codingPlanSetup = AtomCodeBundle.message(locale, "gear.codingPlanSetup"),
        sessionHistory = AtomCodeBundle.message(locale, "gear.sessionHistory"),
        renameSession = AtomCodeBundle.message(locale, "gear.renameSession"),
        deleteSession = AtomCodeBundle.message(locale, "gear.deleteSession"),
        refreshSessions = AtomCodeBundle.message(locale, "gear.refreshSessions"),
        openChanges = AtomCodeBundle.message(locale, "gear.openChanges"),
        diagnostics = AtomCodeBundle.message(locale, "gear.diagnostics"),
        settings = AtomCodeBundle.message(locale, "gear.settings"),
    )
