/* svg2png —— 把 SVG 光栅化成 PNG，作为一个插件。
 *
 * 为什么要它：agent 能写 SVG，但**看不见**自己画的东西。渲染成 PNG 就把回路闭上了
 * ——`read_file` 能显示图片，于是「画 → 看 → 改」不需要浏览器或 GUI 工具链。
 *
 * 依赖 nanosvg（zlib 许可）与 stb_image_write（公有领域），都以头文件形式随仓库存放
 * 在 `vendor/`，构建不需要联网。
 *
 * 用法：
 *   svg2png <in.svg> <out.png> [width]
 *
 * 交互式跑法（经 pluginhost）：`<product>/bin/svg2png a.svg a.png 720`
 */
#include <stdio.h>
#include <stdlib.h>

// 三个库都是「头文件实现」：宏必须先于唯一一次 include 定义。头文件自带 include
// guard，先普通 include 再定义宏二次 include 只会命中 gard、拿不到实现。
#define NANOSVG_IMPLEMENTATION
#include "vendor/nanosvg.h"
#define NANOSVGRAST_IMPLEMENTATION
#include "vendor/nanosvgrast.h"

#define STB_IMAGE_WRITE_IMPLEMENTATION
#include "vendor/stb_image_write.h"

static int usage(void) {
    fprintf(stderr, "usage: svg2png <in.svg> <out.png> [width]\n");
    fprintf(stderr, "  width 省略时按 SVG 自身尺寸（CSS px，96 dpi）输出\n");
    return 2;
}

int plugin_main(int argc, char **argv) {
    if (argc < 3) {
        return usage();
    }
    const char *input = argv[1];
    const char *output = argv[2];
    int target_width = argc > 3 ? atoi(argv[3]) : 0;

    NSVGimage *image = nsvgParseFromFile(input, "px", 96.0f);
    if (image == NULL) {
        fprintf(stderr, "svg2png: cannot parse %s\n", input);
        return 1;
    }

    float scale = 1.0f;
    if (target_width > 0 && image->width > 0.0f) {
        scale = (float)target_width / image->width;
    }
    int width = (int)(image->width * scale + 0.5f);
    int height = (int)(image->height * scale + 0.5f);
    if (width <= 0 || height <= 0) {
        fprintf(stderr, "svg2png: %s has no drawable extent\n", input);
        nsvgDelete(image);
        return 1;
    }

    NSVGrasterizer *rasterizer = nsvgCreateRasterizer();
    if (rasterizer == NULL) {
        fprintf(stderr, "svg2png: cannot allocate the rasterizer\n");
        nsvgDelete(image);
        return 1;
    }
    unsigned char *pixels = calloc((size_t)width * (size_t)height * 4, 1);
    if (pixels == NULL) {
        fprintf(stderr, "svg2png: cannot allocate %dx%d pixels\n", width, height);
        nsvgDeleteRasterizer(rasterizer);
        nsvgDelete(image);
        return 1;
    }

    nsvgRasterize(rasterizer, image, 0, 0, scale, pixels, width, height, width * 4);
    int written = stbi_write_png(output, width, height, 4, pixels, width * 4);

    printf("%s %dx%d\n", output, width, height);

    free(pixels);
    nsvgDeleteRasterizer(rasterizer);
    nsvgDelete(image);
    return written ? 0 : 1;
}
