package com.atomcode.jetbrains.daemon

import java.util.concurrent.CopyOnWriteArrayList

typealias DaemonSupervisorListener = (DaemonSupervisorModel) -> Unit

class DaemonSupervisorStore(
    initialModel: DaemonSupervisorModel = DaemonSupervisorModel(),
    private val retryPolicy: RetryPolicy = RetryPolicy(),
) {
    private val listeners = CopyOnWriteArrayList<DaemonSupervisorListener>()

    @Volatile
    var model: DaemonSupervisorModel = initialModel
        private set

    fun dispatch(action: DaemonSupervisorAction): DaemonSupervisorModel {
        val next = reduceDaemonSupervisor(model, action, retryPolicy)
        model = next
        listeners.forEach { it(next) }
        return next
    }

    fun subscribe(listener: DaemonSupervisorListener): AutoCloseable {
        listeners += listener
        listener(model)
        return AutoCloseable { listeners -= listener }
    }
}
