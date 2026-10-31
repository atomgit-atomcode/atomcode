/*
 * A read-only filesystem MCP server that runs as a pluginhost plugin.
 *
 * This is the "real use" counterpart to `mcp-example`: the example proves the
 * delivery shape (a `.so` with `plugin_main`, protocol on the inherited stdio);
 * this one gives the agent the thing it was missing on a phone — a way to look
 * at files without a shell. Android cannot `execve` a binary from the app's data
 * directory (measured: exit 126), so the server ships exactly like the example:
 * a library loaded by `pluginhost`.
 *
 * Wire format: newline-delimited JSON-RPC 2.0, one message per line. stdout
 * carries protocol messages only; diagnostics go to stderr.
 *
 * Scope and safety: every path argument is resolved (symlinks included) and must
 * stay inside the configured root — `ATOMCODE_MCP_FS_ROOT`, else the process
 * working directory. This server never writes: the tools are `list_dir`,
 * `read_file` and `stat`, all marked readOnlyHint. A path that escapes the root
 * (including via `..` or a symlink) is refused with an error result rather than
 * silently clamped.
 *
 * Deliberately dependency-free: no JSON library, no network, only libc. The
 * parser understands the subset of JSON this server needs (method, id, and
 * string/number arguments), same as `example_server.c`.
 */

#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>

#ifdef _WIN32
#include <direct.h>
#define CANON(buf, path) _fullpath((buf), (path), PATH_MAX)
#else
#define CANON(buf, path) realpath((path), (buf))
#endif

#ifndef PATH_MAX
#define PATH_MAX 4096
#endif

#define MAX_LINE 65536
#define MAX_TEXT (48 * 1024)
#define PROTOCOL_VERSION "2025-11-25"
#define SERVER_NAME "mcp-filesystem"
#define SERVER_VERSION "1.0.0"

/* ------------------------------------------------------------------ JSON bits */

static int json_string(const char *json, const char *key, char *out, size_t cap) {
    char pattern[64];
    snprintf(pattern, sizeof(pattern), "\"%s\"", key);
    const char *found = strstr(json, pattern);
    if (!found) return 0;
    const char *cursor = found + strlen(pattern);
    while (*cursor && *cursor != ':') cursor++;
    if (*cursor != ':') return 0;
    cursor++;
    while (*cursor && isspace((unsigned char)*cursor)) cursor++;
    if (*cursor != '"') return 0;
    cursor++;
    size_t used = 0;
    while (*cursor && *cursor != '"') {
        char c = *cursor++;
        if (c == '\\') {
            char esc = *cursor++;
            switch (esc) {
                case 'n': c = '\n'; break;
                case 't': c = '\t'; break;
                case 'r': c = '\r'; break;
                case '"': c = '"'; break;
                case '\\': c = '\\'; break;
                case '/': c = '/'; break;
                default: c = esc; break;
            }
        }
        if (used + 1 >= cap) break;
        out[used++] = c;
    }
    out[used] = '\0';
    return 1;
}

static int json_long(const char *json, const char *key, long *out) {
    char pattern[64];
    snprintf(pattern, sizeof(pattern), "\"%s\"", key);
    const char *found = strstr(json, pattern);
    if (!found) return 0;
    const char *cursor = found + strlen(pattern);
    while (*cursor && *cursor != ':') cursor++;
    if (*cursor != ':') return 0;
    cursor++;
    while (*cursor && isspace((unsigned char)*cursor)) cursor++;
    if (!isdigit((unsigned char)*cursor) && *cursor != '-') return 0;
    *out = strtol(cursor, NULL, 10);
    return 1;
}

static void json_escape_into(const char *in, char *out, size_t cap) {
    size_t used = 0;
    for (const char *p = in; *p && used + 7 < cap; p++) {
        unsigned char c = (unsigned char)*p;
        switch (c) {
            case '"': out[used++] = '\\'; out[used++] = '"'; break;
            case '\\': out[used++] = '\\'; out[used++] = '\\'; break;
            case '\n': out[used++] = '\\'; out[used++] = 'n'; break;
            case '\r': out[used++] = '\\'; out[used++] = 'r'; break;
            case '\t': out[used++] = '\\'; out[used++] = 't'; break;
            default:
                if (c < 0x20) {
                    used += (size_t)snprintf(out + used, cap - used, "\\u%04x", c);
                } else {
                    out[used++] = (char)c;
                }
                break;
        }
    }
    out[used] = '\0';
}

/* -------------------------------------------------------------------- output */

static void emit(const char *text) {
    fputs(text, stdout);
    fputc('\n', stdout);
    fflush(stdout);
}

/* One result, one line. The escaped form can be up to 6× the raw text (control
 * characters), so both buffers are sized from the text instead of a fixed cap. */
static void emit_text_result(const char *id, const char *text, int is_error) {
    size_t length = strlen(text);
    char *escaped = malloc(length * 6 + 1);
    if (escaped == NULL) {
        emit("{\"jsonrpc\":\"2.0\",\"id\":\"error\",\"error\":{\"code\":-32603,"
             "\"message\":\"out of memory\"}}");
        return;
    }
    json_escape_into(text, escaped, length * 6 + 1);
    size_t needed = strlen(escaped) + 256;
    char *line = malloc(needed);
    if (line == NULL) {
        free(escaped);
        emit("{\"jsonrpc\":\"2.0\",\"id\":\"error\",\"error\":{\"code\":-32603,"
             "\"message\":\"out of memory\"}}");
        return;
    }
    snprintf(
        line,
        needed,
        "{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":[{\"type\":\"text\","
        "\"text\":\"%s\"}],\"isError\":%s}}",
        id,
        escaped,
        is_error ? "true" : "false");
    emit(line);
    free(line);
    free(escaped);
}

/* --------------------------------------------------------------------- paths */

/* Resolve `argument` against the server root and refuse anything that lands
 * outside it. `out` receives an absolute, symlink-free path. */
static int resolve_path(const char *argument, char *out, size_t cap) {
    static char root[PATH_MAX];
    const char *configured = getenv("ATOMCODE_MCP_FS_ROOT");
    if (configured == NULL || configured[0] == '\0') configured = ".";
    if (CANON(root, configured) == NULL) return 0;

    static char joined[PATH_MAX * 2];
    if (argument[0] == '/') {
        snprintf(joined, sizeof(joined), "%s", argument);
    } else {
        snprintf(joined, sizeof(joined), "%s/%s", root, argument);
    }
    static char resolved[PATH_MAX];
    if (CANON(resolved, joined) == NULL) return 0;

    size_t root_len = strlen(root);
    while (root_len > 1 && root[root_len - 1] == '/') root_len--;
    if (strncmp(resolved, root, root_len) != 0) return 0;
    if (resolved[root_len] != '\0' && resolved[root_len] != '/' && resolved[root_len] != '\\') {
        return 0;
    }
    snprintf(out, cap, "%s", resolved);
    return 1;
}

/* --------------------------------------------------------------------- tools */

static const char *TOOLS_JSON =
    "{\"tools\":["
    "{\"name\":\"list_dir\",\"description\":\"List a directory (names, type, size). "
    "Paths are relative to the server root; '..' cannot leave it.\","
    "\"inputSchema\":{\"type\":\"object\",\"properties\":{"
    "\"path\":{\"type\":\"string\",\"description\":\"Directory to list (default '.').\"}},"
    "\"additionalProperties\":false},"
    "\"annotations\":{\"readOnlyHint\":true}},"
    "{\"name\":\"read_file\",\"description\":\"Read a UTF-8 text file. Binary files "
    "and files over the byte limit are refused with an explanation.\","
    "\"inputSchema\":{\"type\":\"object\",\"properties\":{"
    "\"path\":{\"type\":\"string\",\"description\":\"File to read.\"},"
    "\"max_bytes\":{\"type\":\"number\",\"description\":\"Limit, default 65536.\"}},"
    "\"required\":[\"path\"],\"additionalProperties\":false},"
    "\"annotations\":{\"readOnlyHint\":true}},"
    "{\"name\":\"stat\",\"description\":\"Type, size and modification time of a path.\","
    "\"inputSchema\":{\"type\":\"object\",\"properties\":{"
    "\"path\":{\"type\":\"string\",\"description\":\"Path to inspect.\"}},"
    "\"required\":[\"path\"],\"additionalProperties\":false},"
    "\"annotations\":{\"readOnlyHint\":true}}"
    "]}";

static void tool_list_dir(const char *id, const char *arguments) {
    char argument[PATH_MAX];
    if (!json_string(arguments, "path", argument, sizeof(argument))) snprintf(argument, sizeof(argument), ".");
    char path[PATH_MAX];
    if (!resolve_path(argument, path, sizeof(path))) {
        emit_text_result(id, "path is outside the server root, or does not exist", 1);
        return;
    }
    DIR *dir = opendir(path);
    if (dir == NULL) {
        char message[PATH_MAX + 128];
        snprintf(message, sizeof(message), "cannot list %s: %s", argument, strerror(errno));
        emit_text_result(id, message, 1);
        return;
    }
    static char text[MAX_TEXT];
    size_t used = 0;
    text[0] = '\0';
    struct dirent *entry;
    int shown = 0;
    while ((entry = readdir(dir)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) continue;
        char child[PATH_MAX * 2];
        snprintf(child, sizeof(child), "%s/%s", path, entry->d_name);
        struct stat info;
        int line;
        char suffix[32] = "";
        if (stat(child, &info) == 0) {
            if (S_ISDIR(info.st_mode)) {
                snprintf(suffix, sizeof(suffix), "/");
            } else {
                snprintf(suffix, sizeof(suffix), " (%lld B)", (long long)info.st_size);
            }
        }
        line = snprintf(text + used, sizeof(text) - used, "%s%s\n", entry->d_name, suffix);
        if (line < 0 || (size_t)line >= sizeof(text) - used) {
            used = sizeof(text) - 64;
            snprintf(text + used, sizeof(text) - used, "… (listing truncated)\n");
            used = strlen(text);
            break;
        }
        used += (size_t)line;
        shown++;
    }
    closedir(dir);
    if (shown == 0) snprintf(text, sizeof(text), "(empty directory)");
    emit_text_result(id, text, 0);
}

static void tool_read_file(const char *id, const char *arguments) {
    char argument[PATH_MAX];
    if (!json_string(arguments, "path", argument, sizeof(argument))) {
        emit_text_result(id, "read_file requires a string 'path' argument", 1);
        return;
    }
    long limit = 65536;
    json_long(arguments, "max_bytes", &limit);
    if (limit < 1 || limit > (long)MAX_TEXT) limit = MAX_TEXT;

    char path[PATH_MAX];
    if (!resolve_path(argument, path, sizeof(path))) {
        emit_text_result(id, "path is outside the server root, or does not exist", 1);
        return;
    }
    struct stat info;
    if (stat(path, &info) != 0 || !S_ISREG(info.st_mode)) {
        emit_text_result(id, "not a regular file", 1);
        return;
    }
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        char message[PATH_MAX + 128];
        snprintf(message, sizeof(message), "cannot open %s: %s", argument, strerror(errno));
        emit_text_result(id, message, 1);
        return;
    }
    static char text[MAX_TEXT + 1];
    size_t want = (size_t)limit < MAX_TEXT ? (size_t)limit : MAX_TEXT;
    size_t got = fread(text, 1, want, file);
    int more = fgetc(file) != EOF;
    fclose(file);
    for (size_t i = 0; i < got; i++) {
        if (text[i] == '\0') {
            emit_text_result(id, "binary file (contains a NUL byte) — not shown", 1);
            return;
        }
    }
    /* Do not split a UTF-8 sequence at the cut. */
    while (got > 0 && ((unsigned char)text[got] & 0xC0) == 0x80) got--;
    text[got] = '\0';
    if (more) {
        char trailer[64];
        snprintf(trailer, sizeof(trailer), "\n… (truncated at %zu bytes)", got);
        strncat(text, trailer, sizeof(text) - strlen(text) - 1);
    }
    emit_text_result(id, text, 0);
}

static void tool_stat(const char *id, const char *arguments) {
    char argument[PATH_MAX];
    if (!json_string(arguments, "path", argument, sizeof(argument))) {
        emit_text_result(id, "stat requires a string 'path' argument", 1);
        return;
    }
    char path[PATH_MAX];
    if (!resolve_path(argument, path, sizeof(path))) {
        emit_text_result(id, "path is outside the server root, or does not exist", 1);
        return;
    }
    struct stat info;
    if (stat(path, &info) != 0) {
        emit_text_result(id, "cannot stat path", 1);
        return;
    }
    char stamp[32] = "unknown";
    struct tm local;
#ifdef _WIN32
    struct tm *converted = localtime(&info.st_mtime);
    if (converted != NULL) local = *converted;
    else memset(&local, 0, sizeof(local)), stamp[0] = '\0';
#else
    if (localtime_r(&info.st_mtime, &local) != NULL) {
        strftime(stamp, sizeof(stamp), "%Y-%m-%d %H:%M:%S", &local);
    }
#endif
    char text[512];
    snprintf(
        text,
        sizeof(text),
        "%s\n%s, %lld bytes, modified %s",
        path,
        S_ISDIR(info.st_mode) ? "directory" : (S_ISREG(info.st_mode) ? "file" : "other"),
        (long long)info.st_size,
        stamp);
    emit_text_result(id, text, 0);
}

/* ------------------------------------------------------------------- protocol */

static void request_id(const char *json, char *out, size_t cap) {
    const char *found = strstr(json, "\"id\"");
    if (!found) {
        snprintf(out, cap, "null");
        return;
    }
    const char *cursor = found + 4;
    while (*cursor && *cursor != ':') cursor++;
    if (*cursor == ':') cursor++;
    while (*cursor && isspace((unsigned char)*cursor)) cursor++;
    size_t used = 0;
    while (*cursor && used + 1 < cap && strchr("+-0123456789", *cursor)) out[used++] = *cursor++;
    if (used == 0) {
        snprintf(out, cap, "null");
        return;
    }
    out[used] = '\0';
}

static int handle_line(const char *line) {
    char method[64];
    char id[64];
    if (!json_string(line, "method", method, sizeof(method))) return 0;
    int has_id = strstr(line, "\"id\"") != NULL;
    request_id(line, id, sizeof(id));

    if (strcmp(method, "initialize") == 0) {
        char reply[MAX_LINE];
        snprintf(
            reply,
            sizeof(reply),
            "{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"protocolVersion\":\"%s\","
            "\"capabilities\":{\"tools\":{}},"
            "\"serverInfo\":{\"name\":\"%s\",\"version\":\"%s\"},"
            "\"instructions\":\"Read-only filesystem access inside the configured root "
            "(tools: list_dir, read_file, stat).\"}}",
            id,
            PROTOCOL_VERSION,
            SERVER_NAME,
            SERVER_VERSION);
        emit(reply);
        return 1;
    }
    if (strcmp(method, "tools/list") == 0) {
        char reply[MAX_LINE];
        snprintf(reply, sizeof(reply), "{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":%s}", id, TOOLS_JSON);
        emit(reply);
        return 1;
    }
    if (strcmp(method, "tools/call") == 0) {
        char name[64];
        if (!json_string(line, "name", name, sizeof(name))) {
            emit_text_result(id, "missing tool name", 1);
            return 1;
        }
        if (strcmp(name, "list_dir") == 0) {
            tool_list_dir(id, line);
        } else if (strcmp(name, "read_file") == 0) {
            tool_read_file(id, line);
        } else if (strcmp(name, "stat") == 0) {
            tool_stat(id, line);
        } else {
            char message[160];
            snprintf(message, sizeof(message), "unknown tool: %s", name);
            emit_text_result(id, message, 1);
        }
        return 1;
    }
    if (strcmp(method, "ping") == 0) {
        char reply[128];
        snprintf(reply, sizeof(reply), "{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{}}", id);
        emit(reply);
        return 1;
    }
    if (strcmp(method, "resources/list") == 0 || strcmp(method, "prompts/list") == 0) {
        char reply[160];
        snprintf(reply, sizeof(reply), "{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{}}", id);
        emit(reply);
        return 1;
    }
    if (has_id) {
        char reply[256];
        snprintf(
            reply,
            sizeof(reply),
            "{\"jsonrpc\":\"2.0\",\"id\":%s,\"error\":{\"code\":-32601,\"message\":\"method not found: %s\"}}",
            id,
            method);
        emit(reply);
    }
    return 1;
}

/* pluginhost entry point: `argv[0]` is the name the caller typed, `PLUGIN_DIR`
 * names this package's directory. Neither is needed to speak MCP. */
int plugin_main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    static char line[MAX_LINE];
    while (fgets(line, sizeof(line), stdin) != NULL) {
        size_t length = strlen(line);
        while (length > 0 && (line[length - 1] == '\n' || line[length - 1] == '\r')) {
            line[--length] = '\0';
        }
        if (length == 0) continue;
        if (line[0] != '{') continue;
        handle_line(line);
    }
    return 0;
}