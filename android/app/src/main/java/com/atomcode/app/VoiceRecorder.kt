package com.atomcode.app

import android.annotation.SuppressLint
import android.content.Context
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
class VoiceRecorder(private val context: Context) {
    private var record: AudioRecord? = null
    private var pump: Thread? = null
    private var sink: RandomAccessFile? = null
    private var output: File? = null

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

        val dir = File(context.cacheDir, RECORDINGS_DIR).apply { mkdirs() }
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
        recorder.startRecording()
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
        record?.let { recorder ->
            try {
                recorder.stop()
            } catch (error: IllegalStateException) {
                // Stopping a recorder that never started is legal in the docs and
                // throws in practice on some devices; the file still stands.
                Log.w(TAG, "the recorder had already stopped", error)
            }
            recorder.release()
        }
        record = null
        pump?.join(1_000)
        pump = null
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
            return null
        }
        return file
    }

    private fun pumpAudio(recorder: AudioRecord, handle: RandomAccessFile) {
        val buffer = ByteArray(SAMPLE_RATE / 4)
        while (isRecording) {
            val read = recorder.read(buffer, 0, buffer.size)
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
        if (isRecording) stop()
        val file = output
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
