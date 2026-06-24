package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.ui.ChatContextItem
import com.intellij.ui.JBColor
import java.awt.BorderLayout
import java.awt.Component
import java.awt.Cursor
import java.awt.Dimension
import java.awt.Dialog
import java.awt.Image
import java.io.ByteArrayInputStream
import java.util.Base64
import javax.imageio.ImageIO
import javax.swing.BorderFactory
import javax.swing.Box
import javax.swing.BoxLayout
import javax.swing.ImageIcon
import javax.swing.JButton
import javax.swing.JDialog
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.SwingConstants
import javax.swing.SwingUtilities
import kotlin.math.roundToInt

/** Displays files attached to the next prompt as compact, path-aware rows. */
class ContextChipsPanel(
    private val onClear: () -> Unit,
) : JPanel(BorderLayout()) {

    init {
        isOpaque = false
        border = BorderFactory.createEmptyBorder(7, 9, 3, 9)
        isVisible = false
    }

    fun setItems(items: List<ChatContextItem>, onRemove: (ChatContextItem) -> Unit) {
        removeAll()
        isVisible = items.isNotEmpty()
        if (items.isEmpty()) return

        add(buildHeader(items.size), BorderLayout.NORTH)
        add(JPanel().apply {
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            isOpaque = false
            border = BorderFactory.createEmptyBorder(5, 0, 0, 0)
            items.forEachIndexed { index, item ->
                if (index > 0) add(Box.createVerticalStrut(5))
                add(buildAttachmentRow(item, onRemove))
            }
        }, BorderLayout.CENTER)

        revalidate()
        repaint()
    }

    private fun buildHeader(count: Int) = JPanel(BorderLayout()).apply {
        isOpaque = false
        add(JLabel("附件  $count").apply {
            font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 2f)
            foreground = HEADER_FG
        }, BorderLayout.WEST)
        add(makeTextButton("全部移除", onClear), BorderLayout.EAST)
    }

    private fun buildAttachmentRow(
        item: ChatContextItem,
        onRemove: (ChatContextItem) -> Unit,
    ) = JPanel(BorderLayout(8, 0)).apply {
        isOpaque = true
        background = ATTACHMENT_BG
        border = BorderFactory.createCompoundBorder(
            BorderFactory.createLineBorder(ATTACHMENT_BORDER, 1, true),
            BorderFactory.createEmptyBorder(6, 8, 6, 5),
        )
        maximumSize = Dimension(Int.MAX_VALUE, 43)
        toolTipText = item.path

        val normalizedName = item.displayName.replace('\\', '/')
        val fileName = normalizedName.substringAfterLast('/').ifBlank { normalizedName }
        val parentPath = normalizedName.substringBeforeLast('/', "")
        val lineRange = item.startLine?.let { start ->
            val end = item.endLine ?: start
            "  ·  L$start–$end"
        }.orEmpty()

        val imageIcon = decodeImageIcon(item)
        if (imageIcon != null) {
            maximumSize = Dimension(Int.MAX_VALUE, 83)
            val previewLabel = JLabel(scaledIcon(imageIcon, 76, 58)).apply {
                horizontalAlignment = SwingConstants.CENTER
                verticalAlignment = SwingConstants.CENTER
                cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
                toolTipText = "查看大图"
                border = BorderFactory.createLineBorder(ATTACHMENT_BORDER, 1, true)
                preferredSize = Dimension(82, 62)
                addMouseListener(object : java.awt.event.MouseAdapter() {
                    override fun mouseClicked(event: java.awt.event.MouseEvent) {
                        showImagePreview(item, event.component)
                    }
                })
            }
            add(previewLabel, BorderLayout.WEST)
        } else {
            add(JLabel(item.language.uppercase().take(4).ifBlank { "FILE" }).apply {
                horizontalAlignment = JLabel.CENTER
                font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 3f)
                foreground = TYPE_FG
                background = TYPE_BG
                isOpaque = true
                border = BorderFactory.createEmptyBorder(3, 5, 3, 5)
                preferredSize = Dimension(38, 24)
            }, BorderLayout.WEST)
        }

        add(JPanel().apply {
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            isOpaque = false
            add(JLabel(fileName + lineRange).apply {
                font = font.deriveFont(java.awt.Font.BOLD, font.size2D - 1f)
                foreground = FILE_FG
                toolTipText = item.path
            })
            add(JLabel(parentPath.ifBlank { "已附加到下一条消息" }).apply {
                font = font.deriveFont(font.size2D - 3f)
                foreground = PATH_FG
                toolTipText = item.path
            })
        }, BorderLayout.CENTER)

        add(makeTextButton("×") { onRemove(item) }.apply {
            font = font.deriveFont(java.awt.Font.PLAIN, font.size2D + 3f)
            foreground = REMOVE_FG
            toolTipText = "移除附件"
            preferredSize = Dimension(28, 28)
        }, BorderLayout.EAST)
    }

    private fun makeTextButton(text: String, action: () -> Unit) = JButton(text).apply {
        font = font.deriveFont(font.size2D - 2f)
        isContentAreaFilled = false
        isBorderPainted = false
        isFocusPainted = false
        cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
        foreground = ACTION_FG
        margin = java.awt.Insets(0, 4, 0, 4)
        addActionListener { action() }
    }

    private fun decodeImageIcon(item: ChatContextItem): ImageIcon? {
        val mediaType = item.imageMediaType ?: return null
        val data = item.imageData ?: return null
        if (!mediaType.startsWith("image/")) return null
        return runCatching {
            val bytes = Base64.getDecoder().decode(data)
            ImageIcon(ImageIO.read(ByteArrayInputStream(bytes)) ?: return null)
        }.getOrNull()
    }

    private fun scaledIcon(icon: ImageIcon, maxWidth: Int, maxHeight: Int): ImageIcon {
        val width = icon.iconWidth.takeIf { it > 0 } ?: return icon
        val height = icon.iconHeight.takeIf { it > 0 } ?: return icon
        val scale = minOf(maxWidth.toDouble() / width, maxHeight.toDouble() / height, 1.0)
        val scaledWidth = (width * scale).roundToInt().coerceAtLeast(1)
        val scaledHeight = (height * scale).roundToInt().coerceAtLeast(1)
        return ImageIcon(icon.image.getScaledInstance(scaledWidth, scaledHeight, Image.SCALE_SMOOTH))
    }

    private fun showImagePreview(item: ChatContextItem, source: Component) {
        val icon = decodeImageIcon(item) ?: return
        val owner = SwingUtilities.getWindowAncestor(source)
        val dialog = JDialog(owner, item.displayName, Dialog.ModalityType.MODELESS)
        val bounds = source.graphicsConfiguration?.bounds
        val maxWidth = minOf(980, (bounds?.width ?: 1100) - 160).coerceAtLeast(360)
        val maxHeight = minOf(760, (bounds?.height ?: 860) - 180).coerceAtLeast(260)
        dialog.contentPane = JScrollPane(JLabel(scaledIcon(icon, maxWidth, maxHeight)).apply {
            horizontalAlignment = SwingConstants.CENTER
            verticalAlignment = SwingConstants.CENTER
            border = BorderFactory.createEmptyBorder(10, 10, 10, 10)
        }).apply {
            border = BorderFactory.createEmptyBorder()
            preferredSize = Dimension(
                minOf(maxWidth + 22, icon.iconWidth + 22).coerceAtLeast(360),
                minOf(maxHeight + 22, icon.iconHeight + 22).coerceAtLeast(260),
            )
        }
        dialog.defaultCloseOperation = JDialog.DISPOSE_ON_CLOSE
        dialog.pack()
        dialog.setLocationRelativeTo(source)
        dialog.isVisible = true
    }

    companion object {
        private val HEADER_FG = JBColor(0x565A60, 0xA7ABB2)
        private val ACTION_FG = JBColor(0x3574A8, 0x78B6E8)
        private val ATTACHMENT_BG = JBColor(0xF6F9FC, 0x292D32)
        private val ATTACHMENT_BORDER = JBColor(0xD8E2EA, 0x3B424A)
        private val TYPE_BG = JBColor(0xE5F1FA, 0x263D50)
        private val TYPE_FG = JBColor(0x256A99, 0x83C5F1)
        private val FILE_FG = JBColor(0x25282C, 0xE3E5E8)
        private val PATH_FG = JBColor(0x747A82, 0x8E949C)
        private val REMOVE_FG = JBColor(0x777C83, 0x9CA1A8)
    }
}
