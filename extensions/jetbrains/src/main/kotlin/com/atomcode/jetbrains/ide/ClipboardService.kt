package com.atomcode.jetbrains.ide

import java.awt.Toolkit
import java.awt.datatransfer.StringSelection

object ClipboardService {
    fun copyToClipboard(text: String) {
        val clipboard = Toolkit.getDefaultToolkit().systemClipboard
        clipboard.setContents(StringSelection(text), null)
    }
}
