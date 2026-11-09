/* Draws color glyphs through cairo and FreeType, as kitty draws them, in the
 * format crates/felis-shaping/examples/color_reference.rs writes: for each
 * glyph, <out>/<glyph>.img holding "width height left top\n" and then the
 * straight RGBA rows of its ink box, top row first. The glyph origin sits on
 * a whole pixel, as felis places it, so both draw on one pixel grid.
 *
 * cairo loads each COLRv1 layer outline with FT_LOAD_DEFAULT, under which
 * FreeType autohints a TrueType face without instructions, as Noto Color
 * Emoji is. felis draws those outlines unhinted, so FT_Load_Glyph is
 * interposed here (the binary is linked with -rdynamic) to draw them
 * unhinted too: the comparison is of the paint graph, not of hinting.
 *
 * cairo_reference <font> <px> <out> <glyph>...
 */
#define _GNU_SOURCE
#include <cairo-ft.h>
#include <cairo.h>
#include <ft2build.h>
#include FT_FREETYPE_H
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <dlfcn.h>

FT_Error FT_Load_Glyph(FT_Face face, FT_UInt glyph, FT_Int32 flags) {
  static FT_Error (*load)(FT_Face, FT_UInt, FT_Int32);
  if (!load) load = (FT_Error(*)(FT_Face, FT_UInt, FT_Int32))dlsym(RTLD_NEXT, "FT_Load_Glyph");
  return load(face, glyph, flags | FT_LOAD_NO_HINTING);
}

static unsigned char straight(unsigned c, unsigned a) {
  unsigned v = a ? (c * 255 + a / 2) / a : 0;
  return v > 255 ? 255 : (unsigned char)v;
}

static int draw(cairo_font_face_t *face, double px, const char *out, unsigned long glyph) {
  int side = (int)ceil(px * 4), ox = (int)ceil(px), oy = (int)ceil(px * 2.5);
  cairo_surface_t *s = cairo_image_surface_create(CAIRO_FORMAT_ARGB32, side, side);
  cairo_t *cr = cairo_create(s);
  cairo_set_font_face(cr, face);
  cairo_set_font_size(cr, px);
  cairo_font_options_t *o = cairo_font_options_create();
  cairo_font_options_set_color_mode(o, CAIRO_COLOR_MODE_COLOR);
  cairo_font_options_set_antialias(o, CAIRO_ANTIALIAS_GRAY);
  cairo_font_options_set_hint_style(o, CAIRO_HINT_STYLE_NONE);
  cairo_font_options_set_hint_metrics(o, CAIRO_HINT_METRICS_OFF);
  cairo_set_font_options(cr, o);
  /* felis's foreground for a layer that names the text color. */
  cairo_set_source_rgb(cr, 128 / 255.0, 128 / 255.0, 128 / 255.0);
  cairo_glyph_t g = {glyph, ox, oy};
  cairo_show_glyphs(cr, &g, 1);
  cairo_surface_flush(s);
  if (cairo_status(cr) != CAIRO_STATUS_SUCCESS) {
    fprintf(stderr, "glyph %lu: %s\n", glyph, cairo_status_to_string(cairo_status(cr)));
    return 1;
  }
  const unsigned char *data = cairo_image_surface_get_data(s);
  int stride = cairo_image_surface_get_stride(s);
  int x0 = side, y0 = side, x1 = -1, y1 = -1;
  for (int y = 0; y < side; y++)
    for (int x = 0; x < side; x++)
      if (((const unsigned *)(data + y * stride))[x] >> 24) {
        x0 = x < x0 ? x : x0, x1 = x > x1 ? x : x1;
        y0 = y < y0 ? y : y0, y1 = y > y1 ? y : y1;
      }
  if (x1 >= 0 && (x0 == 0 || y0 == 0 || x1 == side - 1 || y1 == side - 1)) {
    fprintf(stderr, "glyph %lu at %g px inks the surface edge\n", glyph, px);
    return 1;
  }
  char path[4096];
  snprintf(path, sizeof path, "%s/%lu.img", out, glyph);
  FILE *f = fopen(path, "wb");
  if (!f) return 1;
  if (x1 < 0) {
    fprintf(f, "0 0 0 0\n");
  } else {
    fprintf(f, "%d %d %d %d\n", x1 - x0 + 1, y1 - y0 + 1, x0 - ox, oy - y0);
    for (int y = y0; y <= y1; y++)
      for (int x = x0; x <= x1; x++) {
        unsigned p = ((const unsigned *)(data + y * stride))[x], a = p >> 24;
        unsigned char rgba[4] = {straight((p >> 16) & 255, a), straight((p >> 8) & 255, a),
                                 straight(p & 255, a), (unsigned char)a};
        fwrite(rgba, 1, 4, f);
      }
  }
  fclose(f);
  cairo_font_options_destroy(o);
  cairo_destroy(cr);
  cairo_surface_destroy(s);
  return 0;
}

int main(int argc, char **argv) {
  if (argc < 5) {
    fprintf(stderr, "usage: cairo_reference <font> <px> <out> <glyph>...\n");
    return 2;
  }
  FT_Library lib;
  FT_Face ft;
  if (FT_Init_FreeType(&lib) || FT_New_Face(lib, argv[1], 0, &ft)) return 1;
  cairo_font_face_t *face = cairo_ft_font_face_create_for_ft_face(ft, FT_LOAD_COLOR);
  double px = atof(argv[2]);
  for (int i = 4; i < argc; i++)
    if (draw(face, px, argv[3], strtoul(argv[i], NULL, 10))) return 1;
  return 0;
}
