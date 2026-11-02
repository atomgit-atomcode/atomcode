package com.atomcode.app

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/**
 * The folder a person granted through the system picker, and the operations the
 * Rust side's SAF world asks of it.
 *
 * Why this exists at all: a Play-distributable build cannot hold
 * `MANAGE_EXTERNAL_STORAGE`, so files outside the app's own directory are only
 * reachable as `content://` documents. Rust's world layer
 * (`crates/atomcode-android/src/saf.rs`) does containment, path composition and
 * error mapping; everything that needs the platform is here, behind method
 * signatures it can call over JNI.
 *
 * Paths crossing that boundary are **relative to the granted tree** and already
 * containment-checked by Rust (empty string is the tree itself). This side never
 * sees a `..`, so it never has to reason about one.
 *
 * Written against [DocumentsContract] rather than `androidx.documentfile` on
 * purpose: that library is one thin convenience layer over exactly these calls,
 * and adding it would mean a new Gradle dependency for a build that has to work
 * offline. Results are JSON rather than exceptions because "the document is
 * gone" is an ordinary answer here, not an error, and an exception crossing JNI
 * turns into a pending-exception check on every call site.
 */
object SafTree {

    private const val TAG = "AtomCode"
    private const val PREFS = "atomcode"
    private const val KEY_TREE = "saf_tree"
    private const val MIME_DIR = DocumentsContract.Document.MIME_TYPE_DIR

    /** The persisted grant, or null when the person has not picked a folder. */
    @Volatile
    private var treeUri: Uri? = null

    @Volatile
    private var context: Context? = null

    /** Tree display name, for logs and for telling the model what it is in. */
    @Volatile
    var displayName: String? = null
        private set

    /** Take a tree URI the picker returned and keep it across restarts. */
    fun attach(ctx: Context, uri: Uri): Boolean = try {
        // Persistable is what survives a reboot; without it the grant lasts until
        // the process dies and the world would vanish underneath a turn.
        ctx.contentResolver.takePersistableUriPermission(
            uri,
            Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION,
        )
        treeUri = uri
        context = ctx.applicationContext
        displayName = DocumentsContract.getTreeDocumentId(uri)
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
            .putString(KEY_TREE, uri.toString()).apply()
        Log.i(TAG, "SAF tree granted: $displayName")
        true
    } catch (failure: Exception) {
        Log.w(TAG, "could not take the SAF grant", failure)
        false
    }

    /** Restore a grant from a previous run, if the platform still honours it. */
    fun restore(ctx: Context) {
        val saved = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(KEY_TREE, null) ?: return
        val uri = runCatching { Uri.parse(saved) }.getOrNull() ?: return
        // A revoked grant throws on the first real query, not here; the world
        // reports it as a refusal at use time, which is the honest place.
        treeUri = uri
        context = ctx.applicationContext
        displayName = runCatching { DocumentsContract.getTreeDocumentId(uri) }.getOrNull()
        Log.i(TAG, "SAF tree restored: $displayName")
    }

    /** Drop the grant (used when the person revokes it in settings). */
    fun detach(ctx: Context) {
        treeUri = null
        displayName = null
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().remove(KEY_TREE).apply()
    }

    fun isReady(): Boolean = treeUri != null

    private fun treeDocumentId(): String? =
        treeUri?.let { runCatching { DocumentsContract.getTreeDocumentId(it) }.getOrNull() }

    private fun error(kind: String, message: String): String =
        JSONObject().put("error", kind).put("message", message).toString()

    /** One row of a directory query: document id, display name and MIME type. */
    private data class Child(val id: String, val name: String, val mime: String) {
        val isDir: Boolean get() = mime == MIME_DIR
    }

    private fun childrenOf(documentId: String): List<Child>? {
        val ctx = context ?: return null
        val root = treeUri ?: return null
        val children = DocumentsContract.buildChildDocumentsUriUsingTree(root, documentId)
        val projection = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
        )
        return try {
            ctx.contentResolver.query(children, projection, null, null, null)?.use { cursor ->
                val out = ArrayList<Child>(cursor.count)
                while (cursor.moveToNext()) {
                    out.add(
                        Child(
                            id = cursor.getString(0),
                            name = cursor.getString(1) ?: continue,
                            mime = cursor.getString(2) ?: "",
                        )
                    )
                }
                out
            }
        } catch (failure: Exception) {
            Log.w(TAG, "SAF query failed for $documentId", failure)
            null
        }
    }

    /**
     * Walk one relative path from the tree root.
     *
     * Each segment is a child query rather than a URI construction: providers
     * mint document ids however they like, so there is no id to derive from a
     * name. Both the id and the MIME type come back, which is what lets `stat`
     * avoid a second round trip.
     */
    private fun resolve(rel: String): Child? {
        val treeId = treeDocumentId() ?: return null
        var current = Child(treeId, "", MIME_DIR)
        for (segment in rel.split('/')) {
            if (segment.isEmpty()) continue
            if (!current.isDir) return null
            current = childrenOf(current.id)?.firstOrNull { it.name == segment } ?: return null
        }
        return current
    }

    private fun documentUri(child: Child): Uri? {
        val root = treeUri ?: return null
        return DocumentsContract.buildDocumentUriUsingTree(root, child.id)
    }

    /** `{"entries":[{"name":…,"isDir":…}]}`, or `{"error":…}`. */
    @JvmStatic
    fun list(rel: String): String {
        val target = resolve(rel) ?: return error("not_found", "no such folder: $rel")
        if (!target.isDir) return error("not_a_directory", "$rel is a file")
        val children = childrenOf(target.id) ?: return error("io", "could not list $rel")
        val entries = JSONArray()
        for (child in children) {
            entries.put(JSONObject().put("name", child.name).put("isDir", child.isDir))
        }
        return JSONObject().put("entries", entries).toString()
    }

    /** The bytes, or null when the document is missing or unreadable. */
    @JvmStatic
    fun read(rel: String): ByteArray? {
        val child = resolve(rel) ?: return null
        if (child.isDir) return null
        val ctx = context ?: return null
        val uri = documentUri(child) ?: return null
        return try {
            ctx.contentResolver.openInputStream(uri)?.use { it.readBytes() }
        } catch (failure: Exception) {
            Log.w(TAG, "SAF read failed for $rel", failure)
            null
        }
    }

    /** `"ok"`, or `{"error":…}`. Creates the document when it is not there. */
    @JvmStatic
    fun write(rel: String, bytes: ByteArray): String {
        val ctx = context ?: return error("denied", "no folder granted")
        val parentRel = rel.substringBeforeLast('/', "")
        val name = rel.substringAfterLast('/')
        val parent = resolve(parentRel) ?: return error("not_found", "no such folder: $parentRel")
        if (!parent.isDir) return error("not_a_directory", "$parentRel is a file")
        return try {
            val existing = childrenOf(parent.id)?.firstOrNull { it.name == name }
            val uri = when {
                existing != null && existing.isDir -> return error("io", "$name is a folder")
                existing != null -> documentUri(existing)
                else -> {
                    val parentUri = documentUri(parent)
                        ?: return error("io", "could not address $parentRel")
                    DocumentsContract.createDocument(
                        ctx.contentResolver, parentUri, "application/octet-stream", name,
                    )
                }
            } ?: return error("io", "could not create $name")
            ctx.contentResolver.openOutputStream(uri, "wt")?.use { it.write(bytes) }
                ?: return error("io", "could not open $name for writing")
            "ok"
        } catch (failure: Exception) {
            error("io", failure.message ?: "write failed")
        }
    }

    /** `"ok"`, or `{"error":…}`. An existing folder of that name is success. */
    @JvmStatic
    fun createDir(rel: String): String {
        val ctx = context ?: return error("denied", "no folder granted")
        val parentRel = rel.substringBeforeLast('/', "")
        val name = rel.substringAfterLast('/')
        val parent = resolve(parentRel) ?: return error("not_found", "no such folder: $parentRel")
        if (!parent.isDir) return error("not_a_directory", "$parentRel is a file")
        return try {
            val existing = childrenOf(parent.id)?.firstOrNull { it.name == name }
            when {
                existing != null && existing.isDir -> "ok"
                existing != null -> error("not_a_directory", "$rel is a file")
                else -> {
                    val parentUri = documentUri(parent)
                        ?: return error("io", "could not address $parentRel")
                    val created = DocumentsContract.createDocument(
                        ctx.contentResolver, parentUri, MIME_DIR, name,
                    )
                    if (created != null) "ok" else error("io", "could not create $rel")
                }
            }
        } catch (failure: Exception) {
            error("io", failure.message ?: "mkdir failed")
        }
    }

    /** `{"isDir":…,"len":…}`, `"null"`, or `{"error":…}`. */
    @JvmStatic
    fun stat(rel: String): String {
        val child = resolve(rel) ?: return "null"
        if (child.isDir) return JSONObject().put("isDir", true).put("len", 0L).toString()
        val ctx = context ?: return "null"
        val uri = documentUri(child) ?: return "null"
        val projection = arrayOf(DocumentsContract.Document.COLUMN_SIZE)
        val len = try {
            ctx.contentResolver.query(uri, projection, null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) cursor.getLong(0) else 0L
            } ?: 0L
        } catch (failure: Exception) {
            0L
        }
        return JSONObject().put("isDir", false).put("len", len).toString()
    }

    /** Tree document id, so the Rust side can name the world it is in. */
    @JvmStatic
    fun treeId(): String = treeDocumentId() ?: ""
}