/*
 * A small MCP (Model Context Protocol) server that runs as a pluginhost plugin.
 *
 * Why a plugin instead of a program: on Android this app may not `execve` anything
 * from its own data directory (measured: exit 126 from the real app process), so a
 * market data package cannot ship a runnable binary. What the platform DOES allow is
 * `dlopen` of a `.so` from that directory — and `pluginhost`, which lives in the
 * APK's nativeLibraryDir, is the only process that needs to be executable. So an MCP
 * server ships as a library with one entry point, `plugin_main`, and the protocol
 * runs on the stdio it inherits.
 *
 * Wire format: newline-delimited JSON-RPC 2.0 — one message per line on stdin,
 * one response per line on stdout. stdout carries protocol messages ONLY (a stray
 * printf breaks the handshake); diagnostics go to stderr.
 *
 * Deliberately dependency-free: no JSON library, no network, only libc. The parser is
 * a small scanner that understands the subset of JSON this server needs — enough to
 * read a method name, an id, and string/number arguments.
 */

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define MAX_LINE 65536
#define PROTOCOL_VERSION "2025-11-25"
#define SERVER_NAME "mcp-example"
#define SERVER_VERSION "1.0.0"

/* ------------------------------------------------------------------ JSON bits */

/* Copy the JSON string value of `key` (with basic escape decoding) into `out`. */
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
                case 'u':
                    /* Not needed by these tools; keep the marker instead of lying. */
                    c = '?';
                    if (cursor[0] && cursor[1] && cursor[2] && cursor[3]) cursor += 4;
                    break;
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

static void emit_text_result(const char *id, const char *text, int is_error) {
    char escaped[MAX_LINE];
    char line[MAX_LINE];
    json_escape_into(text, escaped, sizeof(escaped));
    snprintf(
        line,
        sizeof(line),
        "{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"%s\"}],"
        "\"isError\":%s}}",
        id,
        escaped,
        is_error ? "true" : "false");
    emit(line);
}

/* --------------------------------------------------------------------- tools */

static const char *TOOLS_JSON =
    "{\"tools\":["
    "{\"name\":\"now\",\"description\":\"Current UTC date and time on this device.\","
    "\"inputSchema\":{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false},"
    "\"annotations\":{\"readOnlyHint\":true}},"
    "{\"name\":\"repeat\",\"description\":\"Repeat the given text N times (1..20). "
    "Useful to check how a long string renders or wraps.\","
    "\"inputSchema\":{\"type\":\"object\",\"properties\":{"
    "\"text\":{\"type\":\"string\",\"description\":\"Text to repeat.\"},"
    "\"times\":{\"type\":\"number\",\"description\":\"How many times, 1..20 (default 2).\"}},"
    "\"required\":[\"text\"],\"additionalProperties\":false},"
    "\"annotations\":{\"readOnlyHint\":true}}"
    "]}";

static void tool_now(const char *id) {
    time_t now = time(NULL);
    struct tm utc;
    char stamp[64];
    if (gmtime_r(&now, &utc) == NULL) {
        emit_text_result(id, "could not read the clock", 1);
        return;
    }
    strftime(stamp, sizeof(stamp), "%Y-%m-%d %H:%M:%S", &utc);
    char text[128];
    snprintf(text, sizeof(text), "%s UTC (unix %lld)", stamp, (long long)now);
    emit_text_result(id, text, 0);
}

static void tool_repeat(const char *id, const char *arguments) {
    char text[4096];
    if (!json_string(arguments, "text", text, sizeof(text))) {
        emit_text_result(id, "repeat requires a string 'text' argument", 1);
        return;
    }
    long times = 2;
    json_long(arguments, "times", &times);
    if (times < 1 || times > 20) {
        emit_text_result(id, "times must be between 1 and 20", 1);
        return;
    }
    size_t text_len = strlen(text);
    size_t total = text_len * (size_t)times;
    if (total > 8192) {
        emit_text_result(id, "resulting text would be too long", 1);
        return;
    }
    char *out = malloc(total + 1);
    if (!out) {
        emit_text_result(id, "out of memory", 1);
        return;
    }
    out[0] = '\0';
    for (long i = 0; i < times; i++) {
        strcat(out, text);
        if (i + 1 < times) strcat(out, "\n");
    }
    emit_text_result(id, out, 0);
    free(out);
}

/* ------------------------------------------------------------------- protocol */

/* Pull the raw id token (`123` or `"abc"`) out of the request so it is echoed as-is. */
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
    if (!json_string(line, "method", method, sizeof(method))) return 0; /* notification-ish */
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
            "\"instructions\":\"Example MCP server bundled with AtomCode, built as a "
            "pluginhost library. Read-only tools: now, repeat.\"}}",
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
        if (strcmp(name, "now") == 0) {
            tool_now(id);
        } else if (strcmp(name, "repeat") == 0) {
            tool_repeat(id, line);
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

/* pluginhost entry point: the description of the plugin ABI (in its module docs).
 *
 * `argv[0]` is the name the caller typed (the package name or one of its aliases),
 * `argv[1..]` are the caller's arguments; `PLUGIN_DIR` names this package's
 * directory. None of that is needed to speak MCP, which is the point. */
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
