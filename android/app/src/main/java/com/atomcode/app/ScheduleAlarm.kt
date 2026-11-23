package com.atomcode.app

import android.app.AlarmManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log

/**
 * The one system alarm for the next scheduled task.
 *
 * Why an alarm at all: the daemon sweeps its task list every 30 seconds, but only
 * while the process is alive, and a phone kills a background process whenever it
 * wants to. Scheduled work is the one thing nobody is waiting for, so without a
 * platform wakeup a task simply never runs after the app is killed — the honest
 * limit that used to be written in `docs/android-port.md`.
 *
 * The daemon fires the hook after every sweep ([ScheduleAlarm.wakeAt]); this class
 * turns it into an `AlarmManager` alarm. When it fires, [ScheduleWakeReceiver]
 * starts the agent service, the daemon sweeps, the task runs, and the sweep arms
 * the next alarm. One alarm, replaced in place — never a growing pile.
 *
 * Exactness is a grant, not a given: from Android 12 an app needs "alarms &
 * reminders" (`SCHEDULE_EXACT_ALARM`) for `setExactAndAllowWhileIdle`. Without the
 * grant an inexact alarm still fires; it may just be late, which is reported
 * rather than hidden.
 */
object ScheduleAlarm {
    private const val TAG = "AtomCode"

    /** Identifies this app's single schedule alarm; a constant so it can be replaced. */
    private const val REQUEST_CODE = 3

    const val ACTION_WAKE = "com.atomcode.app.SCHEDULE_WAKE"

    /**
     * The application context, attached by [AgentService] before the daemon can
     * call in. Null means the service never started, and then there is nothing to
     * arm either.
     */
    @Volatile
    private var app: Context? = null

    fun attach(context: Context) {
        app = context.applicationContext
        Log.i(TAG, "schedule alarm bridge attached")
    }

    /** Called from the daemon over JNI with the next due instant, in milliseconds. */
    @JvmStatic
    fun wakeAt(atMillis: Long) {
        val context = app
        if (context == null) {
            // Not an error in itself: the bridge is attached by the service, and the
            // daemon only asks once it is running. Worth a line, because otherwise a
            // silently unarmed alarm looks exactly like "nothing is scheduled".
            Log.w(TAG, "no context yet; cannot arm the schedule alarm")
            return
        }
        val alarms = context.getSystemService(Context.ALARM_SERVICE) as? AlarmManager ?: return
        val pending = pending(context)
        // Never in the past: an alarm set for a moment that already passed fires
        // immediately, and a sweep just computed this instant as future.
        val triggerAt = atMillis.coerceAtLeast(System.currentTimeMillis() + 1_000)
        try {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S || alarms.canScheduleExactAlarms()) {
                alarms.setExactAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, triggerAt, pending)
            } else {
                // No "alarms & reminders" access: this one can be delivered late,
                // but it is delivered — worth more than a task that never fires.
                alarms.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, triggerAt, pending)
                Log.i(TAG, "schedule alarm is inexact (no SCHEDULE_EXACT_ALARM grant)")
            }
            Log.i(TAG, "schedule alarm set for $triggerAt")
        } catch (refused: SecurityException) {
            Log.w(TAG, "cannot set the schedule alarm: ${refused.message}")
        }
    }

    /** Nothing is scheduled: drop the alarm instead of waking up for nothing. */
    @JvmStatic
    fun clear() {
        val context = app ?: return
        val alarms = context.getSystemService(Context.ALARM_SERVICE) as? AlarmManager ?: return
        alarms.cancel(pending(context))
    }

    private fun pending(context: Context): PendingIntent =
        PendingIntent.getBroadcast(
            context,
            REQUEST_CODE,
            Intent(context, ScheduleWakeReceiver::class.java).setAction(ACTION_WAKE),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
}
