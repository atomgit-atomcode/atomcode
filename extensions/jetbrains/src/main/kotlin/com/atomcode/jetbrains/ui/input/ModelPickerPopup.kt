package com.atomcode.jetbrains.ui.input

import com.atomcode.jetbrains.daemon.ModelInfo
import com.intellij.ui.JBColor
import java.awt.AWTEvent
import java.awt.BorderLayout
import java.awt.Color
import java.awt.Component
import java.awt.Dimension
import java.awt.Font
import java.awt.FontMetrics
import java.awt.Graphics
import java.awt.Graphics2D
import java.awt.Point
import java.awt.Rectangle
import java.awt.RenderingHints
import java.awt.Toolkit
import java.awt.event.AWTEventListener
import java.awt.event.MouseAdapter
import java.awt.event.MouseEvent
import javax.swing.BorderFactory
import javax.swing.JComponent
import javax.swing.JScrollPane
import javax.swing.JWindow
import javax.swing.ScrollPaneConstants
import javax.swing.SwingUtilities
import javax.swing.UIManager

/**
 * 模型选择器：可滚动、限高、两行展示（模型名 + provider），默认模型带 default 徽标。
 *
 * 与 `/` 选择器同一套机制：复用常驻 `JWindow`（重开不再新建窗口）、只画可见行、
 * 不抢焦点；区别是点是弹窗外会收起（它是下拉框）。
 */
internal class ModelPickerPopup(
    private val anchor: () -> Component,
    /// 选中回调；由 InputPanel 在打开时装配。
    var onPick: (ModelInfo) -> Unit = {},
    private val onHide: () -> Unit = {},
) {
    private val view = ModelListView { model ->
        onPick(model)
        hide()
    }

    private val scroll = JScrollPane(view).apply {
        border = BorderFactory.createEmptyBorder(0, 0, 0, 0)
        viewport.isOpaque = false
        isOpaque = false
        verticalScrollBarPolicy = ScrollPaneConstants.VERTICAL_SCROLLBAR_AS_NEEDED
        horizontalScrollBarPolicy = ScrollPaneConstants.HORIZONTAL_SCROLLBAR_NEVER
        verticalScrollBar.unitIncrement = ROW_HEIGHT
    }

    private var window: JWindow? = null

    private val surface = javax.swing.JPanel(BorderLayout()).apply {
        border = BorderFactory.createEmptyBorder(SHADOW_MARGIN, SHADOW_MARGIN, SHADOW_MARGIN, SHADOW_MARGIN)
        isOpaque = true
        background = JBColor(Color(0xFF, 0xFF, 0xFF), Color(0x1E, 0x1F, 0x22))
        add(scroll, BorderLayout.CENTER)
    }

    /// 点到弹窗之外就收起（下拉框语义）；点输入框内的模型标签由点击本身切换。
    private val outsideClickListener = AWTEventListener { event: AWTEvent ->
        if (event !is MouseEvent || event.id != MouseEvent.MOUSE_PRESSED) return@AWTEventListener
        val current = window ?: return@AWTEventListener
        if (!current.isVisible) return@AWTEventListener
        val anchorComponent = anchor()
        val inAnchor = Rectangle(0, 0, anchorComponent.width, anchorComponent.height)
            .contains(SwingUtilities.convertPoint(event.component, event.point, anchorComponent))
        val inPopup = SwingUtilities.getWindowAncestor(event.component) === current
        if (!inAnchor && !inPopup) hide()
    }

    val isVisible: Boolean
        get() = window?.isVisible == true

    internal fun popupLocationOnScreen(): Point? = window?.locationOnScreen

    internal fun popupBounds(): Rectangle =
        window?.let { Rectangle(0, 0, it.width, it.height) } ?: Rectangle()

    internal val rows: List<ModelInfo>
        get() = view.models

    internal val selectedIndex: Int
        get() = view.selectedIndex

    fun toggle(models: List<ModelInfo>) {
        if (isVisible) {
            hide()
        } else {
            show(models)
        }
    }

    fun show(models: List<ModelInfo>) {
        val anchorComponent = anchor()
        if (!anchorComponent.isShowing) return
        view.models = models
        if (view.models.isEmpty()) {
            hide()
            return
        }
        val defaultIndex = view.models.indexOfFirst { it.isDefault }
        view.selectedIndex = defaultIndex.takeIf { it >= 0 } ?: 0
        val target = ensureWindow(anchorComponent)
        sizeWindow(target, anchorComponent)
        target.isVisible = true
        target.validate()
        sizeWindow(target, anchorComponent)
        view.ensureSelectionVisible(scroll)
        view.repaint()
        Toolkit.getDefaultToolkit().addAWTEventListener(outsideClickListener, AWTEvent.MOUSE_EVENT_MASK)
    }

    fun hide() {
        val current = window ?: return
        if (!current.isVisible) return
        current.isVisible = false
        Toolkit.getDefaultToolkit().removeAWTEventListener(outsideClickListener)
        onHide()
    }

    /** ↑/↓ 在列表里移动（默认模型起点、环绕）。 */
    fun move(delta: Int): Boolean {
        if (!isVisible || view.models.isEmpty()) return false
        view.move(delta)
        view.ensureSelectionVisible(scroll)
        view.repaint()
        return true
    }

    /** Enter/Tab：选中当前行。 */
    fun activate(): Boolean {
        if (!isVisible) return false
        val model = view.models.getOrNull(view.selectedIndex) ?: return false
        onPick(model)
        hide()
        return true
    }

    private fun ensureWindow(anchorComponent: Component): JWindow {
        window?.let { return it }
        val owner = SwingUtilities.getWindowAncestor(anchorComponent)
        val created = JWindow(owner).apply {
            contentPane.add(surface)
            focusableWindowState = false
        }
        window = created
        return created
    }

    /// 弹窗左沿与锚点对齐、底边压在锚点上沿，并留在屏幕可用区域内。
    private fun sizeWindow(target: JWindow, anchorComponent: Component) {
        if (!anchorComponent.isShowing) return
        val anchorLocation = anchorComponent.locationOnScreen ?: return
        val screen = usableScreenBounds(anchorComponent)
        val width = view.widthFor().coerceIn(MIN_WIDTH, MAX_WIDTH) + 2 * SHADOW_MARGIN
        val available = (anchorLocation.y - screen.y - SCREEN_GAP).coerceAtLeast(ROW_HEIGHT)
        val rows = view.models.size.coerceAtMost(rowsThatFit(available)).coerceAtLeast(1)
        val height = (rows * ROW_HEIGHT + 2 + 2 * SHADOW_MARGIN).coerceAtMost(available)
        // 右沿与控件（圆角边框右侧）对齐，再夹进屏幕
        val anchorRight = anchorLocation.x + anchorComponent.width
        val x = (anchorRight - width).coerceIn(screen.x, (screen.x + screen.width - width).coerceAtLeast(screen.x))
        val y = (anchorLocation.y - height).coerceAtLeast(screen.y)
        target.setBounds(x, y, width, height)
    }

    private fun rowsThatFit(availableHeight: Int): Int = modelRowsThatFit(availableHeight)

    private fun usableScreenBounds(component: Component): Rectangle {
        val usable = java.awt.GraphicsEnvironment.getLocalGraphicsEnvironment().maximumWindowBounds
        val device = component.graphicsConfiguration?.bounds ?: return usable
        val overlap = if (usable.width > 0 && usable.height > 0) usable.intersection(device) else Rectangle()
        return if (overlap.width > 0 && overlap.height > 0) overlap else device
    }

    companion object {
        const val ROW_HEIGHT = 48
        const val MIN_WIDTH = 260
        const val MAX_WIDTH = 520
        const val MIN_ROWS = 3
        const val MAX_ROWS = 12
        const val SHADOW_MARGIN = 4
        const val SCREEN_GAP = 8

        /**
         * 可用高度 → 行数：只取能放下的 4/9（= 先取 2/3 再缩 1/3）。
         * 列表太高会盖住整段对话，宁可靠滚动。
         */
        internal fun modelRowsThatFit(availableHeight: Int, rowHeight: Int = ROW_HEIGHT): Int {
            val usable = (availableHeight - 2 * SHADOW_MARGIN - 2).coerceAtLeast(rowHeight)
            val fit = usable / rowHeight
            return ((fit * 4) / 9).coerceIn(MIN_ROWS, MAX_ROWS)
        }
    }
}

/**
 * 两行一行的模型列表：第一行模型名（加粗），第二行 provider（次要色），默认模型右侧
 * 带 `default` 徽标。只画裁剪区内的行。
 */
internal class ModelListView(
    private val onPick: (ModelInfo) -> Unit,
) : JComponent() {
    var models: List<ModelInfo> = emptyList()
        set(value) {
            val changed = value.size != field.size
            field = value
            selectedIndex = selectedIndex.coerceIn(0, (value.size - 1).coerceAtLeast(0))
            if (changed) revalidate()
            repaint()
        }

    var selectedIndex: Int = 0
        set(value) {
            val clamped = when {
                models.isEmpty() -> 0
                else -> value.coerceIn(0, models.size - 1)
            }
            if (clamped != field) {
                field = clamped
                repaint()
            }
        }

    var allowHover: Boolean = true

    private var hoverIndex: Int = -1
    private var measuredWidth: Int = 0
    private var measuredFor: List<ModelInfo>? = null
    private var metrics: FontMetrics? = null
    private var metricsFont: Font? = null

    init {
        isOpaque = false
        isFocusable = false
        addMouseListener(object : MouseAdapter() {
            override fun mousePressed(event: MouseEvent) {
                indexAt(event.point)?.let {
                    selectedIndex = it
                    repaint()
                }
            }

            override fun mouseReleased(event: MouseEvent) {
                if (!SwingUtilities.isLeftMouseButton(event)) return
                val index = indexAt(event.point) ?: return
                selectedIndex = index
                models.getOrNull(index)?.let(onPick)
            }

            override fun mouseExited(event: MouseEvent) {
                if (hoverIndex != -1) {
                    hoverIndex = -1
                    repaint()
                }
            }
        })
        addMouseMotionListener(object : MouseAdapter() {
            override fun mouseMoved(event: MouseEvent) {
                val index = indexAt(event.point) ?: -1
                if (index != hoverIndex) {
                    hoverIndex = index
                    repaint()
                }
            }
        })
    }

    fun move(delta: Int) {
        if (models.isEmpty()) return
        allowHover = false
        selectedIndex = ((selectedIndex + delta) % models.size + models.size) % models.size
    }

    fun ensureSelectionVisible(scroll: JScrollPane) {
        if (models.isEmpty()) return
        val top = selectedIndex * ModelPickerPopup.ROW_HEIGHT
        scroll.viewport.scrollRectToVisible(Rectangle(0, top, 1, ModelPickerPopup.ROW_HEIGHT))
    }

    fun indexAt(point: Point): Int? {
        if (point.y < 0) return null
        val index = point.y / ModelPickerPopup.ROW_HEIGHT
        return index.takeIf { it in models.indices }
    }

    fun widthFor(): Int {
        if (measuredFor === models && measuredWidth > 0) return measuredWidth
        measuredWidth = measureWidth()
        measuredFor = models
        return measuredWidth
    }

    override fun getPreferredSize(): Dimension =
        Dimension(
            widthFor().coerceIn(ModelPickerPopup.MIN_WIDTH, ModelPickerPopup.MAX_WIDTH),
            models.size * ModelPickerPopup.ROW_HEIGHT,
        )

    override fun paintComponent(graphics: Graphics) {
        if (models.isEmpty()) return
        val g = graphics.create() as Graphics2D
        try {
            g.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON)
            g.setRenderingHint(RenderingHints.KEY_TEXT_ANTIALIASING, RenderingHints.VALUE_TEXT_ANTIALIAS_ON)
            val clip = g.clipBounds
            val first = (clip.y / ModelPickerPopup.ROW_HEIGHT).coerceAtLeast(0)
            val last = ((clip.y + clip.height) / ModelPickerPopup.ROW_HEIGHT).coerceAtMost(models.size - 1)
            if (first > last) return
            val metrics = stableMetrics(g)
            for (index in first..last) {
                val model = models[index]
                val y = index * ModelPickerPopup.ROW_HEIGHT
                if (index == selectedIndex) {
                    paintSelected(g, y)
                } else if (allowHover && index == hoverIndex) {
                    paintHover(g, y)
                }
                paintRow(g, metrics, model, y, index == selectedIndex)
            }
        } finally {
            g.dispose()
        }
    }

    private fun stableMetrics(g: Graphics2D): FontMetrics {
        val font = resolvedFont()
        if (metrics == null || metricsFont !== font) {
            metrics = g.getFontMetrics(font)
            metricsFont = font
        }
        return metrics!!
    }

    private fun paintSelected(g: Graphics2D, y: Int) {
        val width = width - 1
        val height = ModelPickerPopup.ROW_HEIGHT - 2
        for (step in SHADOW_STEPS downTo 1) {
            g.color = Color(0, 0, 0, 14 * step)
            g.fillRoundRect(2, y + 1 + step, width - 4, height, 6, 6)
        }
        g.color = SELECTED_BACKGROUND
        g.fillRoundRect(2, y + 1, width - 4, height, 6, 6)
        g.color = ACCENT
        g.fillRoundRect(2, y + 2, 3, height - 2, 2, 2)
    }

    private fun paintHover(g: Graphics2D, y: Int) {
        g.color = HOVER_BACKGROUND
        g.fillRoundRect(3, y + 2, width - 6, ModelPickerPopup.ROW_HEIGHT - 4, 6, 6)
    }

    private fun paintRow(g: Graphics2D, metrics: FontMetrics, model: ModelInfo, y: Int, selected: Boolean) {
        val base = resolvedFont()
        val titleFont = base.deriveFont(Font.BOLD)
        val subtitleFont = base.deriveFont(base.size2D - 1f)
        g.font = titleFont
        val titleMetrics = g.fontMetrics
        g.color = if (selected) TITLE_SELECTED else TITLE
        g.drawString(model.model, SIDE_PADDING, y + 8 + titleMetrics.ascent)
        g.font = subtitleFont
        val subtitleMetrics = g.fontMetrics
        g.color = if (selected) SUBTITLE_SELECTED else SUBTITLE
        val subtitleY = y + 8 + titleMetrics.ascent + 4 + subtitleMetrics.ascent
        g.drawString(clip(model.provider, subtitleMetrics, width - 2 * SIDE_PADDING - badgeWidth(model)), SIDE_PADDING, subtitleY)
        if (model.isDefault) {
            g.font = base.deriveFont(Font.BOLD, base.size2D - 2f)
            val badgeMetrics = g.fontMetrics
            val text = DEFAULT_BADGE
            val badgeWidth = badgeMetrics.stringWidth(text) + BADGE_PADDING * 2
            val badgeX = width - SIDE_PADDING - badgeWidth
            val badgeY = y + (ModelPickerPopup.ROW_HEIGHT - badgeMetrics.height) / 2
            g.color = BADGE_BACKGROUND
            g.fillRoundRect(badgeX, badgeY, badgeWidth, badgeMetrics.height, 8, 8)
            g.color = BADGE_FOREGROUND
            g.drawString(text, badgeX + BADGE_PADDING, badgeY + badgeMetrics.ascent)
        }
    }

    private fun badgeWidth(model: ModelInfo): Int {
        if (!model.isDefault) return 0
        val metrics = getFontMetrics(resolvedFont().deriveFont(Font.BOLD, resolvedFont().size2D - 2f))
        return metrics.stringWidth(DEFAULT_BADGE) + BADGE_PADDING * 2 + 8
    }

    private fun clip(text: String, metrics: FontMetrics, available: Int): String {
        if (available <= 0) return ""
        if (metrics.stringWidth(text) <= available) return text
        val ellipsis = "…"
        var end = text.length
        while (end > 0 && metrics.stringWidth(text.substring(0, end) + ellipsis) > available) end--
        return text.substring(0, end) + ellipsis
    }

    private fun measureWidth(): Int {
        val base = resolvedFont()
        val titleMetrics = getFontMetrics(base.deriveFont(Font.BOLD))
        val subtitleMetrics = getFontMetrics(base.deriveFont(base.size2D - 1f))
        return models.maxOfOrNull { model ->
            maxOf(
                titleMetrics.stringWidth(model.model),
                subtitleMetrics.stringWidth(model.provider),
            ) + 2 * SIDE_PADDING + badgeWidth(model)
        } ?: ModelPickerPopup.MIN_WIDTH
    }

    private fun resolvedFont(): Font =
        font ?: UIManager.getFont("Label.font") ?: Font("Dialog", Font.PLAIN, 12)

    companion object {
        const val DEFAULT_BADGE = "default"
        private const val SIDE_PADDING = 12
        private const val BADGE_PADDING = 6
        private const val SHADOW_STEPS = 2

        private val SELECTED_BACKGROUND = JBColor(Color(0xE8, 0xF0, 0xFE), Color(0x2D, 0x3B, 0x55))
        private val HOVER_BACKGROUND = JBColor(Color(0xEF, 0xF3, 0xFA), Color(0x30, 0x36, 0x42))
        private val ACCENT = JBColor(Color(0x34, 0x77, 0xE6), Color(0x4C, 0x9A, 0xFF))
        private val TITLE = JBColor(Color(0x1F, 0x23, 0x29), Color(0xE4, 0xE7, 0xEB))
        private val TITLE_SELECTED = JBColor(Color(0x15, 0x54, 0xA6), Color(0xFF, 0xFF, 0xFF))
        private val SUBTITLE = JBColor(Color(0x5B, 0x60, 0x68), Color(0xA8, 0xAD, 0xB6))
        private val SUBTITLE_SELECTED = JBColor(Color(0x33, 0x38, 0x40), Color(0xD6, 0xDB, 0xE3))
        private val BADGE_BACKGROUND = JBColor(Color(0xDC, 0xE7, 0xFA), Color(0x3A, 0x4A, 0x63))
        private val BADGE_FOREGROUND = JBColor(Color(0x1F, 0x5F, 0xB8), Color(0xC9, 0xDD, 0xFF))
    }
}
