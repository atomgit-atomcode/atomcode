/* text2png —— 把文字渲染成 PNG，作为一个插件。
 *
 * 补的是 svg 插件那块缺口：nanosvg 不支持 `<text>`，而让 agent「画一张带中文标注的图」
 * 是常见需求。这里直接用设备上的系统字体（Noto CJK），把一行/多行文字光栅化。
 *
 * 用法：
 *   text2png <out.png> <text> [选项]
 *
 * 选项：
 *   --font   <path>   字体文件（默认 /system/fonts/NotoSansCJK-Regular.ttc）
 *   --size   <px>     字号，默认 32
 *   --fg     <rrggbb> 字色，默认 000000
 *   --bg     <rrggbb|none> 背景色，默认 ffffff；none 表示透明
 *   --width  <px>     换行宽度，默认 720
 *   --padding <px>    内边距，默认 16
 *   --align  left|center|right  默认 left
 *
 * 只依赖 FreeType 与 stb_image_write；字体来自系统，不随包携带。
 */
#include <ft2build.h>
#include FT_FREETYPE_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define STB_IMAGE_WRITE_IMPLEMENTATION
#include "stb_image_write.h"

#define DEFAULT_FONT "/system/fonts/NotoSansCJK-Regular.ttc"
#define MAX_LINES 4096

typedef struct {
    unsigned char r, g, b;
} Color;

typedef struct {
    const char *font_path;
    int size;
    Color fg;
    Color bg;
    int transparent;
    int wrap_width;
    int padding;
    int align; /* 0 left, 1 center, 2 right */
} Options;

static int usage(void) {
    fprintf(stderr,
            "usage: text2png <out.png> <text> [--font PATH] [--size PX] [--fg RRGGBB]\n"
            "                 [--bg RRGGBB|none] [--width PX] [--padding PX]\n"
            "                 [--align left|center|right]\n");
    return 2;
}

static int parse_color(const char *text, Color *out) {
    unsigned int value = 0;
    if (sscanf(text, "%x", &value) != 1 || strlen(text) != 6) {
        return 0;
    }
    out->r = (unsigned char)((value >> 16) & 0xFF);
    out->g = (unsigned char)((value >> 8) & 0xFF);
    out->b = (unsigned char)(value & 0xFF);
    return 1;
}

/* 解码一个 UTF-8 码点；非法序列返回 U+FFFD 并前进 1 字节，绝不越界。 */
static unsigned int utf8_decode(const char *text, int *advance) {
    const unsigned char *bytes = (const unsigned char *)text;
    unsigned int cp;
    if (bytes[0] < 0x80) {
        *advance = 1;
        return bytes[0];
    }
    if ((bytes[0] & 0xE0) == 0xC0 && (bytes[1] & 0xC0) == 0x80) {
        *advance = 2;
        cp = ((unsigned int)(bytes[0] & 0x1F) << 6) | (bytes[1] & 0x3Fu);
        return cp;
    }
    if ((bytes[0] & 0xF0) == 0xE0 && (bytes[1] & 0xC0) == 0x80 && (bytes[2] & 0xC0) == 0x80) {
        *advance = 3;
        cp = ((unsigned int)(bytes[0] & 0x0F) << 12) | ((unsigned int)(bytes[1] & 0x3F) << 6) |
             (bytes[2] & 0x3Fu);
        return cp;
    }
    if ((bytes[0] & 0xF8) == 0xF0 && (bytes[1] & 0xC0) == 0x80 && (bytes[2] & 0xC0) == 0x80 &&
        (bytes[3] & 0xC0) == 0x80) {
        *advance = 4;
        cp = ((unsigned int)(bytes[0] & 0x07) << 18) | ((unsigned int)(bytes[1] & 0x3F) << 12) |
             ((unsigned int)(bytes[2] & 0x3F) << 6) | (bytes[3] & 0x3Fu);
        return cp;
    }
    *advance = 1;
    return 0xFFFD;
}

typedef struct {
    unsigned int *codepoints;
    int *advances; /* 每个码点的水平步进（像素） */
    int count;
    int capacity;
} Text;

static void text_push(Text *text, unsigned int cp, int advance) {
    if (text->count == text->capacity) {
        int grown = text->capacity ? text->capacity * 2 : 256;
        text->codepoints = realloc(text->codepoints, (size_t)grown * sizeof(*text->codepoints));
        text->advances = realloc(text->advances, (size_t)grown * sizeof(*text->advances));
        text->capacity = grown;
    }
    text->codepoints[text->count] = cp;
    text->advances[text->count] = advance;
    text->count++;
}

/* 把文字按宽度折行：返回行数，行边界写进 starts/ends（按码点下标）。 */
static int wrap(const Text *text, int wrap_width, int *starts, int *ends) {
    int lines = 0;
    int line_start = 0;
    int width = 0;
    int last_space = -1;

    for (int index = 0; index < text->count; index++) {
        unsigned int cp = text->codepoints[index];
        if (cp == '\n' || cp == '\r') {
            starts[lines] = line_start;
            ends[lines] = index;
            lines++;
            line_start = index + 1;
            width = 0;
            last_space = -1;
            continue;
        }
        if (lines >= MAX_LINES - 1) {
            break;
        }
        int advance = text->advances[index];
        if (width + advance > wrap_width && index > line_start) {
            /* 拉丁文优先在最近的空格处断行，避免把单词劈开。 */
            int break_at = last_space > line_start ? last_space : index;
            starts[lines] = line_start;
            ends[lines] = break_at;
            lines++;
            line_start = break_at + 1;
            width = 0;
            last_space = -1;
            /* 断点之后的字符重新计入新行宽度。 */
            for (int again = line_start; again <= index; again++) {
                width += text->advances[again];
            }
            if (cp == ' ') {
                continue;
            }
        }
        if (cp == ' ') {
            last_space = index;
        }
        width += advance;
    }
    if (line_start < text->count || lines == 0) {
        starts[lines] = line_start;
        ends[lines] = text->count;
        lines++;
    }
    return lines;
}

static void blend(unsigned char *pixels, int width, int height, int x, int y, const Color *color,
                  unsigned char alpha) {
    if (x < 0 || y < 0 || x >= width || y >= height || alpha == 0) {
        return;
    }
    unsigned char *pixel = pixels + ((size_t)y * (size_t)width + (size_t)x) * 4;
    unsigned int a = alpha;
    pixel[0] = (unsigned char)((color->r * a + pixel[0] * (255 - a)) / 255);
    pixel[1] = (unsigned char)((color->g * a + pixel[1] * (255 - a)) / 255);
    pixel[2] = (unsigned char)((color->b * a + pixel[2] * (255 - a)) / 255);
    pixel[3] = 255;
}

int plugin_main(int argc, char **argv) {
    if (argc < 3) {
        return usage();
    }
    const char *output = argv[1];
    const char *text = argv[2];

    Options options = {
        .font_path = DEFAULT_FONT,
        .size = 32,
        .fg = {0, 0, 0},
        .bg = {255, 255, 255},
        .transparent = 0,
        .wrap_width = 720,
        .padding = 16,
        .align = 0,
    };

    for (int index = 3; index + 1 < argc + 1 && index + 1 <= argc; index += 2) {
        if (index + 1 >= argc) {
            break;
        }
        const char *flag = argv[index];
        const char *value = argv[index + 1];
        if (strcmp(flag, "--font") == 0) {
            options.font_path = value;
        } else if (strcmp(flag, "--size") == 0) {
            options.size = atoi(value);
        } else if (strcmp(flag, "--fg") == 0) {
            if (!parse_color(value, &options.fg)) return usage();
        } else if (strcmp(flag, "--bg") == 0) {
            if (strcmp(value, "none") == 0) {
                options.transparent = 1;
            } else if (!parse_color(value, &options.bg)) {
                return usage();
            }
        } else if (strcmp(flag, "--width") == 0) {
            options.wrap_width = atoi(value);
        } else if (strcmp(flag, "--padding") == 0) {
            options.padding = atoi(value);
        } else if (strcmp(flag, "--align") == 0) {
            options.align = strcmp(value, "center") == 0 ? 1 : strcmp(value, "right") == 0 ? 2 : 0;
        } else {
            fprintf(stderr, "text2png: unknown option %s\n", flag);
            return usage();
        }
    }
    if (options.size <= 0 || options.wrap_width <= 0) {
        return usage();
    }

    FT_Library library;
    if (FT_Init_FreeType(&library) != 0) {
        fprintf(stderr, "text2png: cannot initialise FreeType\n");
        return 1;
    }
    FT_Face face;
    /* ttc 是字体集合，取第 0 个 face（Noto CJK 的简体中文就在里面）。 */
    if (FT_New_Face(library, options.font_path, 0, &face) != 0) {
        fprintf(stderr,
                "text2png: cannot open font %s\n"
                "  hint: 设备上通常有 /system/fonts/NotoSansCJK-Regular.ttc；"
                "用 --font 指定其它字体\n",
                options.font_path);
        FT_Done_FreeType(library);
        return 1;
    }
    if (FT_Set_Pixel_Sizes(face, 0, (FT_UInt)options.size) != 0) {
        fprintf(stderr, "text2png: cannot set size %d\n", options.size);
        FT_Done_Face(face);
        FT_Done_FreeType(library);
        return 1;
    }

    Text body = {0};
    const char *cursor = text;
    while (*cursor) {
        int advance = 1;
        unsigned int cp = utf8_decode(cursor, &advance);
        cursor += advance;
        if (cp == '\n' || cp == '\r') {
            text_push(&body, cp, 0);
            continue;
        }
        int pixels = options.size / 2;
        if (FT_Load_Char(face, cp, FT_LOAD_DEFAULT) == 0) {
            pixels = (int)(face->glyph->advance.x >> 6);
        }
        text_push(&body, cp, pixels);
    }
    if (body.count == 0) {
        fprintf(stderr, "text2png: nothing to render\n");
        FT_Done_Face(face);
        FT_Done_FreeType(library);
        free(body.codepoints);
        free(body.advances);
        return 1;
    }

    int *starts = calloc(MAX_LINES, sizeof(int));
    int *ends = calloc(MAX_LINES, sizeof(int));
    int lines = wrap(&body, options.wrap_width - options.padding * 2, starts, ends);

    int line_height = (int)((float)options.size * 1.45f + 0.5f);
    int width = options.wrap_width;
    int height = lines * line_height + options.padding * 2;
    if (height < options.size + options.padding * 2) {
        height = options.size + options.padding * 2;
    }

    unsigned char *pixels = calloc((size_t)width * (size_t)height * 4, 1);
    if (pixels == NULL) {
        fprintf(stderr, "text2png: cannot allocate %dx%d pixels\n", width, height);
        free(starts);
        free(ends);
        free(body.codepoints);
        free(body.advances);
        FT_Done_Face(face);
        FT_Done_FreeType(library);
        return 1;
    }
    if (!options.transparent) {
        for (size_t index = 0; index < (size_t)width * (size_t)height; index++) {
            pixels[index * 4 + 0] = options.bg.r;
            pixels[index * 4 + 1] = options.bg.g;
            pixels[index * 4 + 2] = options.bg.b;
            pixels[index * 4 + 3] = 255;
        }
    }

    FT_Pos ascent = face->size->metrics.ascender >> 6;
    for (int line = 0; line < lines; line++) {
        int line_width = 0;
        for (int index = starts[line]; index < ends[line]; index++) {
            line_width += body.advances[index];
        }
        int x = options.padding;
        if (options.align == 1) {
            x = (width - line_width) / 2;
        } else if (options.align == 2) {
            x = width - options.padding - line_width;
        }
        int baseline = options.padding + line * line_height + (int)ascent;

        for (int index = starts[line]; index < ends[line]; index++) {
            unsigned int cp = body.codepoints[index];
            if (cp == ' ' || cp == '\n' || cp == '\r') {
                x += body.advances[index];
                continue;
            }
            if (FT_Load_Char(face, cp, FT_LOAD_RENDER) != 0) {
                x += body.advances[index];
                continue;
            }
            FT_GlyphSlot glyph = face->glyph;
            FT_Bitmap *bitmap = &glyph->bitmap;
            int origin_x = x + glyph->bitmap_left;
            int origin_y = baseline - glyph->bitmap_top;
            for (unsigned int row = 0; row < bitmap->rows; row++) {
                for (unsigned int column = 0; column < bitmap->width; column++) {
                    unsigned char alpha = bitmap->buffer[row * bitmap->pitch + column];
                    blend(pixels, width, height, origin_x + (int)column, origin_y + (int)row,
                          &options.fg, alpha);
                }
            }
            x += (int)(glyph->advance.x >> 6);
        }
    }

    int written = stbi_write_png(output, width, height, 4, pixels, width * 4);
    printf("%s %dx%d lines=%d font=%s\n", output, width, height, lines, options.font_path);

    free(pixels);
    free(starts);
    free(ends);
    free(body.codepoints);
    free(body.advances);
    FT_Done_Face(face);
    FT_Done_FreeType(library);
    return written ? 0 : 1;
}
