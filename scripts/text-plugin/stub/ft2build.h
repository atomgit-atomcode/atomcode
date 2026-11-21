/*
 * A minimal FreeType stub — enough to type-check `plugin.c` on a build
 * machine.
 *
 * The plugin renders text with FreeType, which needs the real library and a
 * font file; neither is available on a build machine, and neither is what
 * needs testing. But the plugin is C, and the Rust gates in this repo cannot
 * see a C file at all — so if the plugin stops compiling, nothing else in CI
 * notices. This stub gives `scripts/text-plugin/selftest.sh` a way to run the
 * compiler over the real source without the real dependency.
 *
 * It declares the handful of types and functions `plugin.c` touches, with
 * the shapes the real API has. It is NEVER linked: the syntax check passes
 * `-fsyntax-only`, and the self test compiles only `text_buffer.h` (which
 * has no FreeType in it at all).
 */
#ifndef ATOMCODE_FT2BUILD_STUB_H
#define ATOMCODE_FT2BUILD_STUB_H

typedef long FT_Pos;
typedef unsigned int FT_UInt;
typedef int FT_Int;
typedef int FT_Error;

typedef struct FT_LibraryRec_ *FT_Library;
typedef struct FT_FaceRec_ *FT_Face;
typedef struct FT_SizeRec_ *FT_SizeRec;
typedef struct FT_GlyphSlotRec_ *FT_GlyphSlot;

typedef struct FT_Bitmap_ {
    unsigned int rows;
    unsigned int width;
    int pitch;
    unsigned char *buffer;
} FT_Bitmap;

typedef struct FT_Glyph_Metrics_ {
    int width;
    int height;
} FT_Glyph_Metrics;

typedef struct FT_Size_Metrics_ {
    FT_Pos ascender;
    FT_Pos descender;
} FT_Size_Metrics;

typedef struct FT_GlyphSlotRec_ {
    FT_Bitmap bitmap;
    FT_Glyph_Metrics metrics;
    struct {
        long x, y;
    } advance;
    int bitmap_left;
    int bitmap_top;
} *FT_GlyphSlotRec;

typedef struct FT_SizeRec_ {
    FT_Size_Metrics metrics;
} *FT_SizeRec;

typedef struct FT_FaceRec_ {
    FT_SizeRec size;
    FT_GlyphSlot glyph;
} *FT_FaceRec;

#define FT_LOAD_DEFAULT 0
#define FT_LOAD_RENDER 2

/* `plugin.c` includes the library through this macro, like the real header. */
#define FT_FREETYPE_H <freetype2/FT_FREETYPE_H>

#endif
