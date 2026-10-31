package com.atomcode.app

import java.lang.ref.WeakReference

/**
 * Where the daemon tells Kotlin that a turn started or ended.
 *
 * The runtime knows the turn lifecycle; a poller does not. Measured failure this
 * replaces: the wake lock was driven by a 5-second health poll, released at
 * "0 active turns", and once the CPU slept the poller itself stopped running —
 * so a turn that began just after that point stalled, with the screen off, and
 * its SSE stream died. The daemon now fires a JNI hook on the first turn and
 * again after the last one ends ([AgentService.onTurnActive]).
 *
 * Static methods (`@JvmStatic`) because the JNI side holds only a class
 * reference: an attached native thread cannot look app classes up on its own.
 */
object AgentPower {
    /** Set while the service is alive; cleared in `onDestroy`. */
    @Volatile
    private var service: WeakReference<AgentService>? = null

    @JvmStatic
    fun bind(target: AgentService) {
        service = WeakReference(target)
    }

    @JvmStatic
    fun unbind(target: AgentService) {
        if (service?.get() === target) {
            service = null
        }
    }

    /** Called from the daemon's turn lifecycle. Must stay cheap and non-blocking. */
    @JvmStatic
    fun turnActive(active: Boolean) {
        service?.get()?.onTurnActive(active)
    }
}
