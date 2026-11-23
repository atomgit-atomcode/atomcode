package com.atomcode.app

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.util.Log
import java.io.File
import java.io.RandomAccessFile

/**
 * Record the microphone into a 16 kHz mono WAV.
 *
 * Voice input has three sources and the cheapest one does not need this class:
 * `ACTION_RECOGNIZE_SPEECH` hands the microphone to another app, which returns a
 * sentence and never shares audio. That source is missing on phones whose speech
 * engine is private to the keyboard — measured on PLT140 (OPLUS / Android 16),
 * `query-services -a android.speech.RecognitionService` answers "No services
 * found" — so the audio has to be produced here before a transcription plugin or
 * a model provider can turn it into text.
 *
 * 16 kHz mono 16-bit PCM is the format both of those want: whisper-family engines
 * expect exactly it, and the provider transcription endpoints accept WAV.
 *
 * There is no timer here on purpose. The page owns start and stop, because a
 * recording that stops on its own cuts the person off mid-sentence and there is no
 * way to know how long a sentence is.
 */
class VoiceRecorder(private val directory: File) {
    private var record: AudioRecord? = null
    private var pump: Thread? = null
    private var sink: RandomAccessFile? = null
    private var output: File? = null
    /**
     * The last finished take, kept after [stop] handed it out.
     *
     * `stop()` clears `output` because the file is no longer being written — but
     * the recording is not gone: the page still has to transcribe it, and if it
     * changes its mind the file must be removable. Holding it here is what lets
     * [discard] actually delete something.
     */
    private var lastTake: File? = null

    /** Whether audio is being captured right now. */
    @Volatile
    var isRecording: Boolean = false
        private set

    /**
     * Start capturing, or explain why not.
     *
     * Returns the reason as a word the page can translate rather than a sentence
     * in English: `"busy"`, `"unavailable"`, `"failed"`.
     */
    fun start(): String? {
        if (isRecording) return "busy"
        val rate = SAMPLE_RATE
        val minBuffer = AudioRecord.getMinBufferSize(rate, CHANNEL, ENCODING)
        if (minBuffer <= 0) {
            Log.w(TAG, "this device cannot record at ${rate}Hz (min buffer $minBuffer)")
            return "unavailable"
        }
        val recorder = try {
            AudioRecord(
                MediaRecorder.AudioSource.VOICE_RECOGNITION,
                rate,
                CHANNEL,
                ENCODING,
                maxOf(minBuffer, rate / 2 * 2),
            )
        } catch (error: Exception) {
            Log.w(TAG, "cannot open the microphone", error)
            return "unavailable"
        }
        if (recorder.state != AudioRecord.STATE_INITIALIZED) {
            recorder.release()
            Log.w(TAG, "the microphone did not initialise")
            return "unavailable"
        }

        val dir = File(directory, RECORDINGS_DIR).apply { mkdirs() }
        val file = File(dir, "voice-${System.currentTimeMillis()}.wav")
        val handle = try {
            RandomAccessFile(file, "rw").apply {
                setLength(0)
                // Room for the header; the sizes are patched in when the take ends.
                write(ByteArray(WAV_HEADER_BYTES))
            }
        } catch (error: Exception) {
            recorder.release()
            Log.w(TAG, "cannot write $file", error)
            return "failed"
        }

        record = recorder
        sink = handle
        output = file
        isRecording = true
        try {
            recorder.startRecording()
        } catch (error: Exception) {
            // The recorder opened but could not begin: the microphone may be
            // held by another app, or the device may refuse the source. Leaving
            // the state half-set would make every later `start()` answer
            // "busy" forever, and the exception would otherwise escape into the
            // JavaScript bridge thread and take the process with it.
            Log.w(TAG, "cannot start recording", error)
            isRecording = false
            record = null
            sink = null
            output = null
            runCatching { recorder.release() }
            runCatching { handle.close() }
            file.delete()
            return "unavailable"
        }
        pump = Thread({ pumpAudio(recorder, handle) }, "atomcode-voice").apply {
            isDaemon = true
            start()
        }
        Log.i(TAG, "recording to ${file.name}")
        return null
    }

    /** Stop capturing and return the finished WAV, or null if nothing was captured. */
    fun stop(): File? {
        if (!isRecording) return null
        isRecording = false
        // Order matters: the pump thread is blocked in `recorder.read()`. Releasing
        // the recorder first makes that call throw `IllegalStateException` on a
        // thread with no handler — the exception kills the process, not just the
        // recording. So: stop the recorder (which unblocks `read` with an error
        // return), let the thread notice and exit, and only then release.
        record?.let { recorder ->
            try {
                recorder.stop()
            } catch (error: IllegalStateException) {
                // Stopping a recorder that never started is legal in the docs and
                // throws in practice on some devices; the file still stands.
                Log.w(TAG, "the recorder had already stopped", error)
            }
        }
        // The thread checks `isRecording` between reads, so it exits promptly; the
        // join is bounded in case a read is stuck in the driver.
        pump?.join(2_000)
        pump = null
        record?.release()
        record = null
        val handle = sink
        sink = null
        val file = output
        output = null
        if (handle == null || file == null) return null
        val length = try {
            val size = handle.length()
            writeHeader(handle, size - WAV_HEADER_BYTES)
            handle.close()
            size
        } catch (error: Exception) {
            Log.w(TAG, "cannot finish ${file.name}", error)
            runCatching { handle.close() }
            0L
        }
        if (length <= WAV_HEADER_BYTES) {
            file.delete()
            Log.i(TAG, "the take was empty, so there is nothing to transcribe")
            lastTake = null
            return null
        }
        // Remember it: the page may still change its mind and discard it.
        lastTake = file
        return file
    }

    private fun pumpAudio(recorder: AudioRecord, handle: RandomAccessFile) {
        val buffer = ByteArray(SAMPLE_RATE / 4)
        while (isRecording) {
            val read = try {
                recorder.read(buffer, 0, buffer.size)
            } catch (error: IllegalStateException) {
                // The recorder was released while this read was in flight. That is
                // how `stop()` ends the take; it is not a failure to report, and
                // letting it escape would kill the process.
                Log.w(TAG, "the microphone was closed while reading", error)
                return
            }
            if (read <= 0) {
                if (read < 0) Log.w(TAG, "the microphone returned $read")
                continue
            }
            try {
                handle.write(buffer, 0, read)
            } catch (error: Exception) {
                Log.w(TAG, "cannot append audio", error)
                isRecording = false
                return
            }
        }
    }

    /**
     * Fill in the RIFF sizes now that the take has a length.
     *
     * A WAV header states its own size twice; a streaming encoder would leave both
     * zero, which some decoders reject outright.
     */
    private fun writeHeader(handle: RandomAccessFile, dataBytes: Long) {
        val data = dataBytes.toInt()
        val blockAlign = CHANNELS * (BITS / 8)
        val header = java.nio.ByteBuffer.allocate(WAV_HEADER_BYTES).order(
            java.nio.ByteOrder.LITTLE_ENDIAN,
        )
        header.put("RIFF".toByteArray())
        header.putInt(36 + data)
        header.put("WAVE".toByteArray())
        header.put("fmt ".toByteArray())
        header.putInt(16) // PCM fmt chunk size
        header.putShort(1) // PCM
        header.putShort(CHANNELS.toShort())
        header.putInt(SAMPLE_RATE)
        header.putInt(SAMPLE_RATE * blockAlign)
        header.putShort(blockAlign.toShort())
        header.putShort(BITS.toShort())
        header.put("data".toByteArray())
        header.putInt(data)
        handle.seek(0)
        handle.write(header.array())
        handle.seek(WAV_HEADER_BYTES + data.toLong())
    }

    /** Drop a take that the page never collected (a cancelled recording). */
    fun discard(): Boolean {
        // `stop()` clears `output` (the file is no longer being written) and hands
        // the finished file back. The file to delete is therefore `lastTake` —
        // reading `output` here would find null and silently delete nothing.
        val file = if (isRecording) {
            stop()
            lastTake
        } else {
            output ?: lastTake
        }
        lastTake = null
        output = null
        return file?.delete() ?: false
    }

    companion object {
        private const val TAG = "AtomCode"
        private const val RECORDINGS_DIR = "voice"
        private const val SAMPLE_RATE = 16_000
        private const val CHANNEL = AudioFormat.CHANNEL_IN_MONO
        private const val CHANNELS = 1
        private const val ENCODING = AudioFormat.ENCODING_PCM_16BIT
        private const val BITS = 16
        private const val WAV_HEADER_BYTES = 44
    }
}
