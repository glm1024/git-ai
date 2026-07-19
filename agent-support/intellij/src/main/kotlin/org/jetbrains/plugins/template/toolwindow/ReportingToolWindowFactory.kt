package org.jetbrains.plugins.template.toolwindow

import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.invokeLater
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowFactory
import com.intellij.ui.JBColor
import com.intellij.ui.components.JBLabel
import com.intellij.ui.content.ContentFactory
import com.intellij.util.Alarm
import com.intellij.util.concurrency.AppExecutorUtil
import com.intellij.util.messages.MessageBusConnection
import com.intellij.util.ui.JBUI
import org.jetbrains.plugins.template.reporting.InitialReportingState
import org.jetbrains.plugins.template.reporting.OrganizationLoadResult
import org.jetbrains.plugins.template.reporting.OrganizationOptions
import org.jetbrains.plugins.template.reporting.ReportingProfile
import org.jetbrains.plugins.template.reporting.ReportingProfileListener
import org.jetbrains.plugins.template.reporting.ReportingProfileService
import org.jetbrains.plugins.template.reporting.ReportingProfileTopics
import org.jetbrains.plugins.template.reporting.ReportingSettings
import org.jetbrains.plugins.template.reporting.ReportingSupport
import java.awt.BorderLayout
import java.awt.Component
import java.awt.Dimension
import java.awt.FlowLayout
import java.awt.Cursor
import java.awt.Graphics
import java.awt.Graphics2D
import java.awt.RenderingHints
import java.awt.event.ActionEvent
import java.awt.event.KeyEvent
import java.awt.event.MouseAdapter
import java.awt.event.MouseEvent
import java.awt.event.ComponentAdapter
import java.awt.event.ComponentEvent
import java.awt.event.FocusAdapter
import java.awt.event.FocusEvent
import java.util.concurrent.atomic.AtomicInteger
import javax.swing.Box
import javax.swing.BoxLayout
import javax.swing.AbstractAction
import javax.swing.BorderFactory
import javax.swing.JList
import javax.swing.JPopupMenu
import javax.swing.JScrollPane
import javax.swing.KeyStroke
import javax.swing.JButton
import javax.swing.JComponent
import javax.swing.JPanel
import javax.swing.JSeparator
import javax.swing.JTextField
import javax.swing.ListSelectionModel
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener

class ReportingToolWindowFactory : ToolWindowFactory, DumbAware {
    override fun createToolWindowContent(project: Project, toolWindow: ToolWindow) {
        val panel = ReportingSettingsPanel(project)
        Disposer.register(project, panel)
        val content = ContentFactory.getInstance().createContent(panel, null, false)
        content.isCloseable = false
        toolWindow.contentManager.addContent(content)
    }
}

private fun reportingControlHeight(): Int = JBUI.scale(34)

private data class Choice(
    val value: String,
    val label: String = value,
    val invalid: Boolean = false,
) {
    override fun toString(): String = label
}

private class ClippedStatusLabel(text: String) : JBLabel(text) {
    var maximumTextWidth: Int = Int.MAX_VALUE

    override fun getPreferredSize(): Dimension {
        val preferred = super.getPreferredSize()
        return Dimension(preferred.width.coerceAtMost(maximumTextWidth), preferred.height)
    }
}

private class PrimaryButton(text: String) : JButton(text) {
    init {
        isContentAreaFilled = false
        isBorderPainted = false
        isFocusPainted = false
        isOpaque = false
        border = JBUI.Borders.empty(6, 18)
        cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
    }

    override fun paintComponent(graphics: Graphics) {
        val g = graphics.create() as Graphics2D
        g.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON)
        g.setRenderingHint(RenderingHints.KEY_TEXT_ANTIALIASING, RenderingHints.VALUE_TEXT_ANTIALIAS_ON)
        val background = when {
            model.isPressed -> JBColor(0x0F70B5, 0x0F70B5)
            model.isRollover -> JBColor(0x249BE3, 0x249BE3)
            else -> JBColor(0x168BD2, 0x168BD2)
        }
        g.color = background
        g.fillRoundRect(0, 0, width, height, JBUI.scale(5), JBUI.scale(5))
        g.font = font
        g.color = JBColor.WHITE
        val metrics = g.fontMetrics
        val textX = (width - metrics.stringWidth(text)) / 2
        val textY = (height - metrics.height) / 2 + metrics.ascent
        g.drawString(text, textX, textY)
        g.dispose()
    }
}

private class ReportingSelect : JPanel(BorderLayout()) {
    private val valueLabel = JBLabel()
    private val arrowLabel = JBLabel("⌄")
    private var choices: List<Choice> = emptyList()
    private var selected: Choice = Choice("", "")
    private var onChange: (() -> Unit)? = null
    private var popup: JPopupMenu? = null

    init {
        isOpaque = true
        background = JBColor.namedColor("TextField.background", JBColor(0xFFFFFF, 0x252526))
        isFocusable = true
        cursor = Cursor.getPredefinedCursor(Cursor.HAND_CURSOR)
        updateBorder(focused = false)
        preferredSize = Dimension(JBUI.scale(220), reportingControlHeight())
        minimumSize = Dimension(JBUI.scale(80), reportingControlHeight())
        maximumSize = Dimension(Int.MAX_VALUE, reportingControlHeight())
        add(valueLabel, BorderLayout.CENTER)
        add(arrowLabel, BorderLayout.EAST)
        val open = { if (isEnabled) togglePopup() }
        val openListener = object : MouseAdapter() {
            override fun mousePressed(event: MouseEvent) {
                requestFocusInWindow()
                open()
            }
        }
        addMouseListener(openListener)
        valueLabel.addMouseListener(openListener)
        arrowLabel.addMouseListener(openListener)
        addFocusListener(object : FocusAdapter() {
            override fun focusGained(event: FocusEvent) = updateBorder(focused = true)
            override fun focusLost(event: FocusEvent) {
                if (popup?.isVisible != true) updateBorder(focused = false)
            }
        })
        inputMap.put(KeyStroke.getKeyStroke(KeyEvent.VK_SPACE, 0), "open")
        inputMap.put(KeyStroke.getKeyStroke(KeyEvent.VK_ENTER, 0), "open")
        inputMap.put(KeyStroke.getKeyStroke(KeyEvent.VK_DOWN, 0), "open")
        actionMap.put("open", object : AbstractAction() {
            override fun actionPerformed(event: ActionEvent) = open()
        })
    }

    fun addChangeListener(listener: () -> Unit) { onChange = listener }

    fun setChoices(placeholder: String, values: List<String>, selectedValue: String) {
        choices = values.map(::Choice)
        selected = when {
            selectedValue.isBlank() -> Choice("", placeholder)
            choices.any { it.value == selectedValue } -> choices.first { it.value == selectedValue }
            else -> Choice(selectedValue, "$selectedValue（已失效）", invalid = true)
        }
        valueLabel.text = selected.label
        valueLabel.foreground = if (selected.value.isBlank()) JBColor.GRAY else JBColor.foreground()
        toolTipText = selected.label
        closePopup()
        repaint()
    }

    fun selectedValue(): String = selected.value

    override fun setEnabled(enabled: Boolean) {
        super.setEnabled(enabled)
        valueLabel.isEnabled = enabled
        arrowLabel.isEnabled = enabled
        cursor = Cursor.getPredefinedCursor(if (enabled) Cursor.HAND_CURSOR else Cursor.DEFAULT_CURSOR)
    }

    private fun togglePopup() {
        if (popup?.isVisible == true) closePopup() else showPopup()
    }

    private fun showPopup() {
        if (choices.isEmpty()) return
        updateBorder(focused = true)
        arrowLabel.text = "⌃"
        val list = JList(choices.toTypedArray()).apply {
            selectionMode = ListSelectionModel.SINGLE_SELECTION
            setSelectedValue(choices.firstOrNull { it.value == selected.value }, true)
            fixedCellHeight = JBUI.scale(32)
            border = JBUI.Borders.empty(4)
            cellRenderer = javax.swing.ListCellRenderer { _, choice, _, isSelected, _ ->
                JPanel(BorderLayout()).apply {
                    isOpaque = true
                    background = if (isSelected) JBColor(0xE8F2FC, 0x094771) else JBColor.background()
                    border = JBUI.Borders.empty(0, 8)
                    add(JBLabel(choice.label), BorderLayout.CENTER)
                    if (choice.value == selected.value) add(JBLabel("✓"), BorderLayout.EAST)
                }
            }
        }
        val menu = JPopupMenu().apply {
            border = JBUI.Borders.customLine(
                JBColor.namedColor("Focus.color", JBColor(0x4B89DC, 0x0E639C)),
                1,
            )
            addPopupMenuListener(object : javax.swing.event.PopupMenuListener {
                override fun popupMenuWillBecomeVisible(event: javax.swing.event.PopupMenuEvent) = Unit
                override fun popupMenuWillBecomeInvisible(event: javax.swing.event.PopupMenuEvent) {
                    arrowLabel.text = "⌄"
                    popup = null
                    updateBorder(focused = hasFocus())
                }
                override fun popupMenuCanceled(event: javax.swing.event.PopupMenuEvent) {
                    arrowLabel.text = "⌄"
                    popup = null
                    updateBorder(focused = hasFocus())
                }
            })
        }
        val visibleRows = choices.size.coerceAtMost(8)
        val popupWidth = width.coerceAtLeast(JBUI.scale(160))
        menu.add(JScrollPane(list).apply {
            border = null
            horizontalScrollBarPolicy = JScrollPane.HORIZONTAL_SCROLLBAR_NEVER
            verticalScrollBarPolicy = if (choices.size > visibleRows) JScrollPane.VERTICAL_SCROLLBAR_AS_NEEDED else JScrollPane.VERTICAL_SCROLLBAR_NEVER
            preferredSize = Dimension(popupWidth, visibleRows * JBUI.scale(32) + JBUI.scale(8))
        })
        val commit = {
            list.selectedValue?.let { choice ->
                if (choice.value != selected.value) {
                    selected = choice
                    valueLabel.text = choice.label
                    valueLabel.foreground = JBColor.foreground()
                    onChange?.invoke()
                }
            }
            closePopup()
        }
        list.addMouseListener(object : MouseAdapter() {
            override fun mouseReleased(event: MouseEvent) = commit()
        })
        list.inputMap.put(KeyStroke.getKeyStroke(KeyEvent.VK_ENTER, 0), "commit")
        list.actionMap.put("commit", object : AbstractAction() {
            override fun actionPerformed(event: ActionEvent) = commit()
        })
        popup = menu
        menu.show(this, 0, height + JBUI.scale(3))
        list.requestFocusInWindow()
    }

    private fun closePopup() {
        popup?.isVisible = false
        popup = null
        arrowLabel.text = "⌄"
        updateBorder(focused = hasFocus())
    }

    private fun updateBorder(focused: Boolean) {
        val color = if (focused) {
            JBColor.namedColor("Focus.color", JBColor(0x4B89DC, 0x0E639C))
        } else {
            JBColor.namedColor("Component.borderColor", JBColor(0xA8ADB4, 0x46606D))
        }
        border = BorderFactory.createCompoundBorder(
            JBUI.Borders.customLine(color, 1),
            JBUI.Borders.empty(0, 10),
        )
        repaint()
    }
}

private class ReportingSettingsPanel(@Suppress("unused") private val project: Project) : JPanel(BorderLayout()), Disposable {
    private val service = ReportingProfileService()
    private val departmentCombo = ReportingSelect()
    private val officeCombo = ReportingSelect()
    private val teamCombo = ReportingSelect()
    private val nameField = JTextField()
    private val emailField = JTextField()
    private val serverField = JTextField()
    private val nameControl = framedTextField(nameField)
    private val emailControl = framedTextField(emailField)
    private val serverControl = framedTextField(serverField)
    private val statusLabel = ClippedStatusLabel("正在加载配置…")
    private val saveButton = PrimaryButton("保存")
    private lateinit var teamField: JPanel
    private lateinit var headerPanel: JPanel
    private lateinit var headerTitle: JBLabel
    private lateinit var headerActions: JPanel
    private var compactHeader: Boolean? = null
    private val reloadAlarm = Alarm(Alarm.ThreadToUse.POOLED_THREAD, this)
    private val organizationRequest = AtomicInteger()
    private val connection: MessageBusConnection = ApplicationManager.getApplication().messageBus.connect(this)

    private var organizationOptions: OrganizationOptions? = null
    private var updatingControls = false
    private var dirty = false
    private var saving = false
    private var cliError: String? = null
    @Volatile private var disposed = false

    init {
        border = JBUI.Borders.empty(18, 18, 22, 18)
        add(createContent(), BorderLayout.NORTH)
        addComponentListener(object : ComponentAdapter() {
            override fun componentResized(event: ComponentEvent) {
                updateHeaderLayout()
            }
        })
        bindInteractions()
        connection.subscribe(ReportingProfileTopics.PROFILE_SAVED, ReportingProfileListener { settings ->
            invokeLater {
                if (disposed) return@invokeLater
                if (dirty || saving) {
                    setStatus("配置已在其他窗口保存；当前编辑尚未保存", Status.WARNING)
                } else {
                    applySettings(settings)
                    refreshOrganization(settings.metricsApiBaseUrl)
                    setStatus("已保存", Status.SAVED)
                }
            }
        })
        loadInitialState()
    }

    private fun createContent(): JPanel {
        val root = JPanel(BorderLayout()).apply { isOpaque = false }
        val body = verticalPanel()
        headerPanel = JPanel().apply {
            isOpaque = false
            alignmentX = Component.LEFT_ALIGNMENT
        }
        headerTitle = JBLabel("数据上报").apply {
            font = font.deriveFont(font.style, JBUI.scale(24).toFloat())
        }
        headerActions = JPanel(FlowLayout(FlowLayout.RIGHT, JBUI.scale(12), 0)).apply {
            isOpaque = false
            add(statusLabel)
            add(saveButton)
        }
        statusLabel.toolTipText = statusLabel.text
        updateHeaderLayout(forceCompact = true)

        teamField = createField("组", teamCombo)
        body.add(headerPanel)
        body.add(verticalGap(16))
        body.add(divider())
        body.add(verticalGap(18))
        body.add(createCluster(
            createField("部门", departmentCombo),
            createField("处", officeCombo),
            teamField,
        ))
        body.add(verticalGap(18))
        body.add(divider())
        body.add(verticalGap(18))
        body.add(createCluster(
            createField("姓名", nameControl),
            createField("公司邮箱", emailControl),
        ))
        body.add(verticalGap(18))
        body.add(divider())
        body.add(verticalGap(18))
        body.add(createField("上报服务器地址", serverControl))
        body.add(verticalGap(6))
        body.add(JBLabel("用于接收 AI 生成代码统计数据。").apply {
            foreground = JBColor.namedColor("GitAi.Reporting.help", JBColor(0x6B7078, 0xA7ADB7))
            alignmentX = Component.LEFT_ALIGNMENT
        })
        root.add(body, BorderLayout.NORTH)
        return root
    }

    private fun updateHeaderLayout(forceCompact: Boolean? = null) {
        if (!::headerPanel.isInitialized) return
        val contentWidth = (width - insets.left - insets.right).coerceAtLeast(0)
        val compact = forceCompact ?: (contentWidth in 1 until JBUI.scale(340))
        val statusWidth = (contentWidth - saveButton.preferredSize.width - JBUI.scale(24))
            .coerceIn(JBUI.scale(72), JBUI.scale(260))
        statusLabel.maximumTextWidth = statusWidth
        if (compactHeader == compact && forceCompact == null) {
            headerActions.revalidate()
            headerActions.repaint()
            return
        }
        compactHeader = compact
        headerPanel.removeAll()
        if (compact) {
            headerPanel.layout = BoxLayout(headerPanel, BoxLayout.Y_AXIS)
            headerPanel.add(fullWidthRow(headerTitle, BorderLayout.WEST))
            headerPanel.add(verticalGap(10))
            headerPanel.add(fullWidthRow(headerActions, BorderLayout.EAST))
        } else {
            headerPanel.layout = BorderLayout()
            headerPanel.add(headerTitle, BorderLayout.WEST)
            headerPanel.add(headerActions, BorderLayout.EAST)
        }
        headerPanel.maximumSize = Dimension(Int.MAX_VALUE, headerPanel.preferredSize.height)
        headerPanel.revalidate()
        headerPanel.repaint()
    }

    private fun fullWidthRow(component: JComponent, constraint: String): JPanel = JPanel(BorderLayout()).apply {
        isOpaque = false
        alignmentX = Component.LEFT_ALIGNMENT
        add(component, constraint)
        maximumSize = Dimension(Int.MAX_VALUE, preferredSize.height)
    }

    private fun verticalPanel(): JPanel = JPanel().apply {
        layout = BoxLayout(this, BoxLayout.Y_AXIS)
        isOpaque = false
        alignmentX = Component.LEFT_ALIGNMENT
        maximumSize = Dimension(Int.MAX_VALUE, Int.MAX_VALUE)
    }

    private fun createCluster(vararg fields: JPanel): JPanel = verticalPanel().apply {
        fields.forEachIndexed { index, field ->
            if (index > 0) add(verticalGap(14))
            add(fullWidthRow(field, BorderLayout.CENTER))
        }
    }

    private fun createField(label: String, component: JComponent): JPanel = verticalPanel().apply {
        add(JBLabel(label).apply {
            font = font.deriveFont(font.style)
            alignmentX = Component.LEFT_ALIGNMENT
        })
        add(verticalGap(6))
        component.maximumSize = Dimension(Int.MAX_VALUE, component.preferredSize.height)
        add(fullWidthRow(component, BorderLayout.CENTER))
        maximumSize = Dimension(Int.MAX_VALUE, preferredSize.height)
    }

    private fun framedTextField(field: JTextField): JPanel {
        val visualInset = JBUI.scale(4)
        val frame = object : JPanel(null) {
            override fun doLayout() {
                // Material Theme 把可见边框画在组件边界内侧；向两边扩展以让可见边框与标签共线。
                field.setBounds(
                    -visualInset,
                    -visualInset,
                    width + visualInset * 2,
                    height + visualInset * 2,
                )
            }
        }.apply {
            isOpaque = false
            preferredSize = Dimension(JBUI.scale(220), reportingControlHeight())
            minimumSize = Dimension(JBUI.scale(80), reportingControlHeight())
            maximumSize = Dimension(Int.MAX_VALUE, reportingControlHeight())
            add(field)
        }
        field.foreground = JBColor.foreground()
        field.caretColor = JBColor.foreground()
        field.selectionColor = JBColor.namedColor("TextField.selectionBackground", JBColor(0xA6D2FF, 0x094771))
        return frame
    }

    private fun divider(): JSeparator = JSeparator().apply {
        foreground = JBColor.namedColor("GitAi.Reporting.divider", JBColor(0x2F6DB5, 0x2F6DB5))
        alignmentX = Component.LEFT_ALIGNMENT
        maximumSize = Dimension(Int.MAX_VALUE, 1)
    }

    private fun verticalGap(value: Int): Component = (Box.createVerticalStrut(JBUI.scale(value)) as JComponent)
        .apply { alignmentX = Component.LEFT_ALIGNMENT }

    private fun bindInteractions() {
        saveButton.addActionListener { save() }
        departmentCombo.addChangeListener {
            if (!updatingControls) {
                updateDependentChoices(clearOffice = true, clearTeam = true)
                markDirty()
            }
        }
        officeCombo.addChangeListener {
            if (!updatingControls) {
                updateDependentChoices(clearOffice = false, clearTeam = true)
                markDirty()
            }
        }
        teamCombo.addChangeListener { if (!updatingControls) markDirty() }
        val fieldListener = object : DocumentListener {
            override fun insertUpdate(event: DocumentEvent) = changed(event)
            override fun removeUpdate(event: DocumentEvent) = changed(event)
            override fun changedUpdate(event: DocumentEvent) = changed(event)
            private fun changed(event: DocumentEvent) {
                if (updatingControls) return
                markDirty()
                if (event.document == serverField.document) scheduleOrganizationRefresh()
            }
        }
        nameField.document.addDocumentListener(fieldListener)
        emailField.document.addDocumentListener(fieldListener)
        serverField.document.addDocumentListener(fieldListener)
    }

    private fun loadInitialState() {
        AppExecutorUtil.getAppExecutorService().execute {
            val state = service.loadInitialState()
            invokeLater {
                if (disposed) return@invokeLater
                applyInitialState(state)
            }
        }
    }

    private fun applyInitialState(state: InitialReportingState) {
        organizationOptions = state.organization.options
        cliError = state.cliError
        applySettings(state.settings)
        when {
            state.cliError != null -> setStatus(state.cliError, Status.ERROR)
            state.organization.error != null -> setStatus(state.organization.error, Status.WARNING)
            state.importedFields.isNotEmpty() -> {
                dirty = true
                setStatus("有未保存修改", Status.DIRTY)
            }
            else -> setStatus("已保存", Status.SAVED)
        }
    }

    private fun applySettings(settings: ReportingSettings) {
        updatingControls = true
        try {
            nameField.text = settings.profile.userName
            emailField.text = settings.profile.userEmail
            serverField.text = settings.metricsApiBaseUrl
            updateChoices(settings)
        } finally {
            updatingControls = false
        }
        dirty = false
        updateSaveButton()
    }

    private fun updateDependentChoices(clearOffice: Boolean, clearTeam: Boolean) {
        val before = readSettings()
        val settings = before.copy(profile = before.profile.copy(
            officeName = if (clearOffice) "" else before.profile.officeName,
            teamName = if (clearTeam) "" else before.profile.teamName,
        ))
        updatingControls = true
        try {
            updateChoices(settings, updateDepartment = false)
        } finally {
            updatingControls = false
        }
    }

    private fun scheduleOrganizationRefresh() {
        val url = serverField.text
        val request = organizationRequest.incrementAndGet()
        reloadAlarm.cancelAllRequests()
        reloadAlarm.addRequest({
            val result = service.loadOrganizationOptions(url)
            invokeLater {
                if (disposed || request != organizationRequest.get() || serverField.text != url) return@invokeLater
                applyOrganizationResult(result)
            }
        }, 300)
    }

    private fun refreshOrganization(url: String) {
        val request = organizationRequest.incrementAndGet()
        AppExecutorUtil.getAppExecutorService().execute {
            val result = service.loadOrganizationOptions(url)
            invokeLater {
                if (disposed || request != organizationRequest.get()) return@invokeLater
                applyOrganizationResult(result)
            }
        }
    }

    private fun applyOrganizationResult(result: OrganizationLoadResult) {
        organizationOptions = result.options
        val current = readSettings()
        applyChoicesPreservingCurrent(current)
        if (result.error != null) setStatus(result.error, Status.WARNING)
    }

    private fun applyChoicesPreservingCurrent(settings: ReportingSettings) {
        updatingControls = true
        try {
            updateChoices(settings)
        } finally {
            updatingControls = false
        }
    }

    private fun updateChoices(settings: ReportingSettings, updateDepartment: Boolean = true) {
        val departmentNames = organizationOptions?.departments?.map { it.name }.orEmpty()
        val offices = ReportingSupport.offices(settings, organizationOptions)
        val teams = ReportingSupport.teams(settings, organizationOptions)
        if (updateDepartment) departmentCombo.setChoices("请选择部门", departmentNames, settings.profile.departmentName)
        officeCombo.setChoices(if (settings.profile.departmentName.isBlank()) "请先选择部门" else "请选择处", offices.map { it.name }, settings.profile.officeName)
        teamCombo.setChoices("请选择组", teams, settings.profile.teamName.takeIf { teams.isNotEmpty() }.orEmpty())
        departmentCombo.isEnabled = organizationOptions != null
        officeCombo.isEnabled = organizationOptions != null && settings.profile.departmentName.isNotBlank()
        teamCombo.isEnabled = organizationOptions != null && settings.profile.officeName.isNotBlank() && teams.isNotEmpty()
        teamField.isVisible = teams.isNotEmpty()
        teamField.parent?.revalidate()
        teamField.parent?.repaint()
    }

    private fun readSettings(): ReportingSettings = ReportingSettings(
        metricsApiBaseUrl = serverField.text,
        profile = ReportingProfile(
            departmentName = departmentCombo.selectedValue(),
            officeName = officeCombo.selectedValue(),
            teamName = teamCombo.selectedValue(),
            userName = nameField.text,
            userEmail = emailField.text,
        ),
    )

    private fun markDirty() {
        if (saving) return
        dirty = true
        setStatus("有未保存修改", Status.DIRTY)
        updateSaveButton()
    }

    private fun save() {
        if (saving) return
        if (cliError != null) {
            setStatus(cliError!!, Status.ERROR)
            return
        }
        val settings = readSettings()
        val validationError = ReportingSupport.validate(settings, organizationOptions)
        if (validationError != null) {
            setStatus(validationError, Status.ERROR)
            return
        }
        saving = true
        updateSaveButton()
        AppExecutorUtil.getAppExecutorService().execute {
            runCatching { service.save(settings) }.onSuccess { saved ->
                ApplicationManager.getApplication().messageBus.syncPublisher(ReportingProfileTopics.PROFILE_SAVED).profileSaved(saved)
                invokeLater {
                    if (disposed) return@invokeLater
                    saving = false
                    applySettings(saved)
                    setStatus("已保存", Status.SAVED)
                }
            }.onFailure { error ->
                invokeLater {
                    if (disposed) return@invokeLater
                    saving = false
                    updateSaveButton()
                    setStatus(error.message ?: "保存失败，请稍后重试", Status.ERROR)
                }
            }
        }
    }

    private fun setStatus(message: String, status: Status) {
        statusLabel.toolTipText = message
        statusLabel.isVisible = true
        statusLabel.text = when (status) {
            Status.SAVED -> "● $message"
            Status.DIRTY -> "● $message"
            else -> message
        }
        statusLabel.foreground = when (status) {
            Status.SAVED -> JBColor.namedColor("GitAi.Reporting.success", JBColor(0x3D9B6D, 0x6FCF97))
            Status.DIRTY -> JBColor.namedColor("GitAi.Reporting.dirty", JBColor(0xC58A22, 0xE3A94E))
            Status.ERROR -> JBColor.namedColor("GitAi.Reporting.error", JBColor(0xC75450, 0xF48771))
            Status.WARNING -> JBColor.namedColor("GitAi.Reporting.warning", JBColor(0xC58A22, 0xE3A94E))
        }
        updateHeaderLayout()
    }

    private fun updateSaveButton() {
        // 与 VS Code 页面保持一致：按钮始终保持主按钮视觉；保存中通过文字和状态阻止重复提交。
        saveButton.isEnabled = true
    }

    override fun dispose() {
        disposed = true
    }

    private enum class Status { SAVED, DIRTY, ERROR, WARNING }
}
