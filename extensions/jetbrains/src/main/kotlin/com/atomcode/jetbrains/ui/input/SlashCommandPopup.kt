package com.atomcode.jetbrains.ui.input

import com.intellij.ui.JBColor
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
import java.awt.Window
import java.awt.event.MouseAdapter
import java.awt.event.MouseEvent
import javax.swing.BorderFactory
import javax.swing.JComponent
import javax.swing.JPopupMenu
import javax.swing.JScrollPane
import javax.swing.ScrollPaneConstants
import javax.swing.SwingUtilities
import javax.swing.UIManager

/**
 * The `/` picker.
 *
 * - 向上弹出，**底边贴着输入框上沿**（按屏幕坐标摆位，不靠 `show(invoker, -h)`）；
 * - 高度只吃"输入框上方真正放得下"的空间，超出部分进滚动区（滚轮 / ↑↓）；
 * - 选中行有圆角填充 + 左侧强调条 + 柔和阴影；
 * - 焦点一直留在输入框，所以打字就是过滤，Enter/Tab 插入、Esc 与点击别处收起。
 */
internal class SlashCommandPopup(
    private val anchor: () -> Component,
    private val onInsert: (String) -> Unit,
    /// 宽度与左侧都用它：默认跟 [anchor]，调用方给"输入框外框"才能与输入框同宽对齐。
    private val horizontalAnchor: () -> Component = anchor,
    /// 弹窗收起时回调（点外面、Esc、过滤无结果都算）：调用方据此复位触发状态，
    /// 否则"焦点移开→收起→继续输入"就再也不匹配了。
    private val onHide: () -> Unit = {},
) {
    private val view = CommandListView { command ->
        onInsert(command.name)
        hide()
    }

    private val scroll = JScrollPane(view).apply {
        border = BorderFactory.createEmptyBorder(0, 0, 0, 0)
        viewport.isOpaque = false
        isOpaque = false
        verticalScrollBarPolicy = ScrollPaneConstants.VERTICAL_SCROLLBAR_AS_NEEDED
        horizontalScrollBarPolicy = ScrollPaneConstants.HORIZONTAL_SCROLLBAR_NEVER
        verticalScrollBar.unitIncrement = ROW_HEIGHT
        horizontalScrollBar.unitIncrement = ROW_HEIGHT
    }

    /// 复用同一个窗口：`JPopupMenu.show()` 每次都新建窗口（实测首开 56ms、重开 18ms），
    /// 而一个常驻的 `JWindow` 让重开降到个位数毫秒。
    private var window: javax.swing.JWindow? = null

    private val surface = javax.swing.JPanel(BorderLayout()).apply {
        border = BorderFactory.createEmptyBorder(SHADOW_MARGIN, SHADOW_MARGIN, SHADOW_MARGIN, SHADOW_MARGIN)
        isOpaque = true
        background = JBColor(Color(0xFF, 0xFF, 0xFF), Color(0x1E, 0x1F, 0x22))
        add(scroll, BorderLayout.CENTER)
    }

    private fun ensureWindow(anchorComponent: Component): javax.swing.JWindow {
        window?.let { return it }
        val owner = SwingUtilities.getWindowAncestor(anchorComponent)
        val created = javax.swing.JWindow(owner).apply {
            contentPane.add(surface)
            // 不抢焦点：键盘输入必须留在输入框里（打字即过滤）。
            focusableWindowState = false
        }
        window = created
        return created
    }

    private var commands: List<SlashCommand> = emptyList()

    val isVisible: Boolean
        get() = window?.isVisible == true

    internal val selectedIndex: Int
        get() = view.selectedIndex

    internal val visibleCommands: List<SlashCommand>
        get() = view.items

    internal fun popupBounds(): Rectangle =
        window?.let { Rectangle(0, 0, it.width, it.height) } ?: Rectangle()

    /** 弹窗在屏幕上的位置，测试用来验证"贴输入框上沿"。 */
    internal fun popupLocationOnScreen(): java.awt.Point? = window?.locationOnScreen

    fun show(all: List<SlashCommand>, prefix: String) {
        val anchorComponent = anchor()
        if (!anchorComponent.isShowing) return
        val sameList = all === commands
        commands = all
        if (isVisible && sameList) {
            // 已经开着同一份清单：只更新过滤结果，不重开窗口。
            update(prefix)
            return
        }
        view.items = SlashCommands.match(all, prefix)
        if (view.items.isEmpty()) {
            hide()
            return
        }
        view.selectedIndex = 0
        sizePopup(view.items.size.coerceAtMost(rowsThatFit(anchorComponent)).coerceAtLeast(1))
        val target = ensureWindow(anchorComponent)
        // 先把窗口摆到估算位置并显示，再按平台真实给出的高度校正一次：窗口管理器
        // 可能改动尺寸，压到输入框上就白搭了。
        reposition(anchorComponent, target)
        target.isVisible = true
        target.validate()
        reposition(anchorComponent, target)
        view.repaint()
    }

    fun update(prefix: String) {
        if (!isVisible) return
        view.items = SlashCommands.match(commands, prefix)
        view.selectedIndex = 0
        if (view.items.isEmpty()) {
            hide()
            return
        }
        // 关键：**不重排窗口**。每次按键都 pack/setSize 会让重型弹窗重排一次，
        // 表现就是打字和移动光标都卡。
        view.ensureSelectionVisible(scroll)
        view.repaint()
    }

    /// 技能是异步取回的（vscode 也是打开时才 ensureSlashSkills）：到达后就地替换列表。
    fun updateCommands(all: List<SlashCommand>, prefix: String) {
        commands = all
        if (!isVisible) return
        view.items = SlashCommands.match(all, prefix)
        view.selectedIndex = 0
        view.widthFor(remeasure = true)
        val rows = view.items.size.coerceAtMost(rowsThatFit(anchor())).coerceAtLeast(1)
        sizePopup(rows)
        window?.let { reposition(anchor(), it) }
        view.repaint()
    }

    /** Moves the highlight, wrapping like the VS Code picker. */
    fun move(delta: Int): Boolean {
        if (!isVisible || view.items.isEmpty()) return false
        // 用键盘浏览时不再跟随鼠标，直到鼠标真的动过（vscode 的 allow-hover）。
        view.allowHover = false
        view.move(delta)
        view.ensureSelectionVisible(scroll)
        view.repaint()
        return true
    }

    /** Inserts the highlighted command; false when nothing is highlighted. */
    fun activate(): Boolean {
        if (!isVisible) return false
        val command = view.items.getOrNull(view.selectedIndex) ?: return false
        onInsert(command.name)
        hide()
        return true
    }

    fun hide() {
        val current = window
        val wasVisible = current?.isVisible == true
        if (wasVisible) {
            current!!.isVisible = false
        }
        // 无论窗口是否已建出来，只要收起过一次就通知调用方复位触发状态。
        if (wasVisible || window == null) onHide()
    }

    /// 弹窗宽度 = 输入框宽度（vscode 的选择器与输入框同宽）。
    private fun popupWidth(): Int {
        val box = horizontalAnchor()
        val boxWidth = if (box.isShowing) box.width else 0
        return boxWidth.takeIf { it >= MIN_WIDTH } ?: view.widthFor(remeasure = false).coerceIn(MIN_WIDTH, MAX_WIDTH)
    }

    private fun sizePopup(rows: Int) {
        val width = popupWidth()
        view.preferredSize = Dimension(width, rows * ROW_HEIGHT)
        scroll.preferredSize = Dimension(width, rows * ROW_HEIGHT + 2)
    }

    /// 底边压在输入框上沿，并整体留在屏幕内。
    private fun reposition(anchorComponent: Component, target: java.awt.Window) {
        if (!anchorComponent.isShowing) return
        val anchorLocation = anchorComponent.locationOnScreen ?: return
        val screen = screenBoundsOf(anchorComponent)
        val width = popupWidth()
        val available = (anchorLocation.y - screen.y - SCREEN_GAP).coerceAtLeast(ROW_HEIGHT)
        // 自然高度必须用"限高后的行数"，否则这里会把 rowsThatFit 的预算又撑回去。
        val rows = view.items.size.coerceAtMost(rowsThatFit(anchorComponent)).coerceAtLeast(1)
        val naturalHeight = rows * ROW_HEIGHT + 2 + 2 * SHADOW_MARGIN
        // 以窗口"当前真实高度"为准（显示后可能被平台改过），再用可用空间夹一次。
        val height = target.height.takeIf { it > 0 }?.coerceAtMost(available)
            ?: naturalHeight.coerceAtMost(available)
        // 左边与输入框（外框）左沿对齐，而不是锚点自身的 x。
        val leftEdge = horizontalAnchor().takeIf { it.isShowing }?.locationOnScreen?.x
            ?: anchorLocation.x
        val x = leftEdge.coerceIn(screen.x, (screen.x + screen.width - width).coerceAtLeast(screen.x))
        val y = (anchorLocation.y - height).coerceAtLeast(screen.y)
        target.setBounds(x, y, width, height)
        view.ensureSelectionVisible(scroll)
    }

    /// 输入框上方真正放得下多少行（不是整屏），这样弹窗永远贴着输入框上沿。
    private fun rowsThatFit(anchorComponent: Component): Int {
        val screen = screenBoundsOf(anchorComponent)
        val anchorTop = if (anchorComponent.isShowing) {
            anchorComponent.locationOnScreen?.y ?: (screen.y + screen.height)
        } else {
            screen.y + screen.height
        }
        // 只用输入框上方可用空间的一半：列表太高会盖住整段对话，也让人读不过来。
        val available = ((anchorTop - screen.y - SCREEN_GAP) / 2).coerceAtMost(screen.height / 2)
        return commandRowsThatFit(available, ROW_HEIGHT)
    }

    /// 用"可用窗口区域"而不是整块屏幕：macOS 会把窗口 y 坐标抬到菜单栏以下，
    /// 用整块屏幕算出来的位置会被系统改掉，底边就压到输入框上了。
    private fun screenBoundsOf(component: Component): Rectangle {
        val usable = java.awt.GraphicsEnvironment.getLocalGraphicsEnvironment().maximumWindowBounds
        val device = component.graphicsConfiguration?.bounds
        if (device == null) return usable
        val overlap = if (usable.width > 0 && usable.height > 0) usable.intersection(device) else Rectangle()
        return if (overlap.width > 0 && overlap.height > 0) overlap else device
    }

    companion object {
        const val ROW_HEIGHT = 34
        const val DEFAULT_VISIBLE_ROWS = 12
        const val MIN_WIDTH = 420
        const val MAX_WIDTH = 900
        const val SHADOW_MARGIN = 4
        const val SCREEN_GAP = 8
    }
}

/**
 * Paints the command rows. One component for all rows: hit testing, hover and the
 * selected-row shadow stay in one place, and only the rows inside the clip bounds
 * are painted — a 120-command list repainted in full is what made scrolling crawl.
 */
internal class CommandListView(
    private val onPick: (SlashCommand) -> Unit,
) : JComponent() {
    var items: List<SlashCommand> = emptyList()
        set(value) {
            val heightChanged = value.size != field.size
            field = value
            // 过滤后变短时选中不能越界，否则 Enter/Tab 会"点了没反应"。
            selectedIndex = selectedIndex.coerceIn(0, (value.size - 1).coerceAtLeast(0))
            if (heightChanged) revalidate()  // 行数没变就别惊动布局
            descriptionCache.clear()
            repaint()
        }

    var selectedIndex: Int = 0
        set(value) {
            val clamped = when {
                items.isEmpty() -> 0
                else -> value.coerceIn(0, items.size - 1)
            }
            if (clamped != field) {
                field = clamped
                repaint()
            }
        }

    /// 键盘浏览后为 false：此时不跟随鼠标，直到鼠标真的移动（vscode 的 allow-hover）。
    var allowHover: Boolean = true

    private var hoverIndex: Int = -1
    private val descriptionCache = HashMap<Long, String>()
    private var measuredWidth: Int = 0
    private var measuredFor: List<SlashCommand>? = null
    private var metricsCache: FontMetrics? = null
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
                items.getOrNull(index)?.let(onPick)
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
                allowHover = true
                val index = indexAt(event.point) ?: -1
                if (index != hoverIndex) {
                    hoverIndex = index
                    repaint()
                }
            }
        })
    }

    fun selectedName(): String? = items.getOrNull(selectedIndex)?.name

    fun move(delta: Int) {
        if (items.isEmpty()) return
        selectedIndex = ((selectedIndex + delta) % items.size + items.size) % items.size
    }

    fun ensureSelectionVisible(scroll: JScrollPane) {
        if (items.isEmpty()) return
        val top = selectedIndex * SlashCommandPopup.ROW_HEIGHT
        scroll.viewport.scrollRectToVisible(Rectangle(0, top, 1, SlashCommandPopup.ROW_HEIGHT))
    }

    fun indexAt(point: Point): Int? {
        if (point.y < 0) return null
        val index = point.y / SlashCommandPopup.ROW_HEIGHT
        return index.takeIf { it in items.indices }
    }

    /// 宽度只在清单换了一份时量一次：每敲一个字符都量 100+ 行是打开慢的主因之一。
    fun widthFor(remeasure: Boolean): Int {
        if (!remeasure && measuredFor === items && measuredWidth > 0) return measuredWidth
        measuredWidth = measureWidth()
        measuredFor = items
        return measuredWidth
    }

    fun preferredWidth(): Int = widthFor(remeasure = true)

    override fun getPreferredSize(): Dimension =
        Dimension(
            widthFor(remeasure = false).coerceIn(SlashCommandPopup.MIN_WIDTH, SlashCommandPopup.MAX_WIDTH),
            items.size * SlashCommandPopup.ROW_HEIGHT,
        )

    override fun paintComponent(graphics: Graphics) {
        if (items.isEmpty()) return
        val g = graphics.create() as Graphics2D
        try {
            g.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON)
            g.setRenderingHint(RenderingHints.KEY_TEXT_ANTIALIASING, RenderingHints.VALUE_TEXT_ANTIALIAS_ON)
            val clip = g.clipBounds
            val first = (clip.y / SlashCommandPopup.ROW_HEIGHT).coerceAtLeast(0)
            val last = ((clip.y + clip.height) / SlashCommandPopup.ROW_HEIGHT).coerceAtMost(items.size - 1)
            if (first > last) return
            val metrics = stableMetrics(g)
            for (index in first..last) {
                val command = items[index]
                val y = index * SlashCommandPopup.ROW_HEIGHT
                if (index == selectedIndex) {
                    paintSelectedRow(g, y)
                } else if (allowHover && index == hoverIndex) {
                    paintHoverRow(g, y)
                }
                paintText(g, metrics, command, index, y, index == selectedIndex)
            }
        } finally {
            g.dispose()
        }
    }

    private fun stableMetrics(g: Graphics2D): FontMetrics {
        val font = resolvedFont()
        val cached = metricsCache
        if (cached != null && metricsFont === font) return cached
        val metrics = g.getFontMetrics(font)
        metricsCache = metrics
        metricsFont = font
        return metrics
    }

    private fun paintSelectedRow(g: Graphics2D, y: Int) {
        val inset = 2
        val arc = 6
        val width = width - 1
        val height = SlashCommandPopup.ROW_HEIGHT - 2
        // 抬起感的阴影：选中块下方两圈半透明描边，越靠下越淡。
        for (step in SHADOW_STEPS downTo 1) {
            g.color = Color(0, 0, 0, 14 * step)
            g.fillRoundRect(inset, y + 1 + step, width - 2 * inset, height, arc, arc)
        }
        g.color = SELECTED_BACKGROUND
        g.fillRoundRect(inset, y + 1, width - 2 * inset, height, arc, arc)
        // 左侧强调条：对齐 vscode 的 inset 2px accent。
        g.color = ACCENT
        g.fillRoundRect(inset, y + 2, 3, height - 2, 2, 2)
    }

    private fun paintHoverRow(g: Graphics2D, y: Int) {
        g.color = HOVER_BACKGROUND
        g.fillRoundRect(3, y + 2, width - 6, SlashCommandPopup.ROW_HEIGHT - 4, 6, 6)
    }

    private fun paintText(
        g: Graphics2D,
        metrics: FontMetrics,
        command: SlashCommand,
        index: Int,
        y: Int,
        selected: Boolean,
    ) {
        val base = resolvedFont()
        val baseline = y + (SlashCommandPopup.ROW_HEIGHT + metrics.ascent - metrics.descent) / 2
        g.font = base.deriveFont(Font.BOLD)
        g.color = if (selected) LABEL_SELECTED else LABEL
        g.drawString(command.name, SIDE_PADDING, baseline)
        val labelWidth = g.fontMetrics.stringWidth(command.name)
        val descriptionX = SIDE_PADDING + labelWidth + TEXT_GAP
        val available = width - descriptionX - SIDE_PADDING
        if (available > 0) {
            g.font = base
            g.color = if (selected) DESCRIPTION_SELECTED else DESCRIPTION
            g.drawString(clippedDescription(index, command.description, metrics, available), descriptionX, baseline)
        }
    }

    /// 省略号截断结果按"行 + 可用宽度"缓存：paint 里逐字符量字体是大头。
    private fun clippedDescription(index: Int, text: String, metrics: FontMetrics, available: Int): String {
        val key = (index.toLong() shl 32) or available.toLong()
        descriptionCache[key]?.let { return it }
        val clipped = if (metrics.stringWidth(text) <= available) {
            text
        } else {
            val ellipsis = "…"
            var end = text.length
            while (end > 0 && metrics.stringWidth(text.substring(0, end) + ellipsis) > available) end--
            text.substring(0, end) + ellipsis
        }
        descriptionCache[key] = clipped
        return clipped
    }

    private fun measureWidth(): Int {
        val metrics = metricsCache ?: getFontMetrics(resolvedFont())
        return items.maxOfOrNull { command ->
            metrics.stringWidth(command.name) + metrics.stringWidth(command.description) + TEXT_GAP + 2 * SIDE_PADDING
        } ?: SlashCommandPopup.MIN_WIDTH
    }

    /// 未显示过的组件 font 可能为 null（构建期/测试里就会），退到主题默认字体。
    private fun resolvedFont(): Font =
        font ?: UIManager.getFont("Label.font") ?: Font("Dialog", Font.PLAIN, 12)

    companion object {
        private const val SIDE_PADDING = 12
        private const val TEXT_GAP = 14
        private const val SHADOW_STEPS = 2

        private val SELECTED_BACKGROUND = JBColor(Color(0xE8, 0xF0, 0xFE), Color(0x2D, 0x3B, 0x55))
        private val HOVER_BACKGROUND = JBColor(Color(0xEF, 0xF3, 0xFA), Color(0x30, 0x36, 0x42))
        private val ACCENT = JBColor(Color(0x34, 0x77, 0xE6), Color(0x4C, 0x9A, 0xFF))
        private val LABEL = JBColor(Color(0x1F, 0x6F, 0xD1), Color(0x6C, 0xB1, 0xFF))
        private val LABEL_SELECTED = JBColor(Color(0x15, 0x54, 0xA6), Color(0x9A, 0xC8, 0xFF))
        private val DESCRIPTION = JBColor(Color(0x5B, 0x60, 0x68), Color(0xA8, 0xAD, 0xB6))
        private val DESCRIPTION_SELECTED = JBColor(Color(0x33, 0x38, 0x40), Color(0xD6, 0xDB, 0xE3))
    }
}
