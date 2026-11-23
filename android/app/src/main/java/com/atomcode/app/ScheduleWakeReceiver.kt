package com.atomcode.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * The system alarm for a due scheduled task, ringing in a process that may not exist.
 *
 * All this does is start the agent service: the daemon then sweeps its task list,
 * runs whatever is due and re-arms the next alarm. Keeping the decision there means
 * one implementation of "what is due" (`schedule_runner`), shared with the in-process
 * sweep — a receiver that decided for itself would be a second one.
 */
class ScheduleWakeReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ScheduleAlarm.ACTION_WAKE) return
        Log.i(TAG, "schedule alarm fired; starting the agent service")
        // The alarm can arrive before any activity exists in this process, so the
        // alarm class has nothing attached yet.
        ScheduleAlarm.attach(context)
        // `start` already catches a refusal from Android 12's background
        // foreground-service restriction and logs it: nothing here can do better,
        // and a crashed receiver would take the alarm's delivery report with it.
        AgentService.start(context)
    }

    private companion object {
        const val TAG = "AtomCode"
    }
}
